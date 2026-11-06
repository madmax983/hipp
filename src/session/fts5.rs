//! FTS5 in the session (v0.16, leaf `fts5`): the pin's `fts5_main.c`
//! and `fts5_storage.c` over `crate::fts5`'s index core.
//!
//! - **CREATE / DROP**: the shadow tables `%_data`, `%_idx` (WITHOUT
//!   ROWID), `%_content`, `%_docsize`, `%_config` (WITHOUT ROWID) with
//!   the pin's text, in its order, and their first rows.
//! - **Writes** (`xUpdate`): content and docsize rows, the pending
//!   terms of each row's tokens (deletes tokenize the old content),
//!   the totals, the special INSERT commands. The pending terms and
//!   the totals live in the connection for the transaction
//!   ([`FtState`]) and are flushed where the pin flushes them: at
//!   commit, at each `SAVEPOINT`, at the statement savepoint of a
//!   multi-row write inside a transaction, at a `RELEASE` that ends
//!   a level they were flushed under, when a write's rowid runs
//!   backwards, and past `hashsize` bytes. `ROLLBACK [TO]` drops
//!   them.
//! - **Reads** (`xFilter`): MATCH queries over the segments and the
//!   pending terms, the rank, the auxiliary functions' per-row data
//!   (`Env::fts_rows`), full scans and rowid lookups of the content;
//!   fts5vocab tables.

use super::vtab::VtPlan;
use super::*;
use crate::fts5::aux::{AuxRow, AuxTab};
use crate::fts5::config::{Config, Content, Runtime};
use crate::fts5::expr::Expr as FtExpr;
use crate::fts5::{Detail, Hash, Op as FtOp, Store};
use crate::record::KeyOrder;
use crate::vtab::{VTab, VtSrc};
use alloc::rc::Rc;

fn ft_err<E>(kind: SchemaKind) -> Error<E> {
    Error::Schema { kind }
}

fn corrupt<E>() -> Error<E> {
    ft_err(SchemaKind::VtabCorrupt)
}

/// A table's place in the open transaction: the pin's pending terms
/// (`Fts5Index`), totals (`Fts5Storage`) and savepoint depth
/// (`Fts5FullTable::iSavepoint`).
#[derive(Debug, Clone)]
pub(super) struct FtState {
    name: AVec<u8>,
    hash: Hash,
    write_rowid: i64,
    bdelete: bool,
    nrows: u64,
    /// `nTotalRow` and `aTotalSize[]`, while loaded.
    totals: Option<(i64, AVec<i64>)>,
    sp: usize,
    /// The pager's rollback count when the state began: a rollback
    /// since makes it stale.
    epoch: u64,
    /// The cached structure record a tombstone write changed
    /// (`Fts5Index::pStruct`), written at the next flush; readers see
    /// it in place of `%_data`'s.
    #[cfg(feature = "fts5_delete")]
    pstruct: Option<AVec<u8>>,
    /// `nContentlessDelete`.
    #[cfg(feature = "fts5_delete")]
    ncdel: u64,
}

impl FtState {
    fn new(name: AVec<u8>, detail: Detail, sp: usize, epoch: u64) -> FtState {
        FtState {
            name,
            hash: Hash::new(detail),
            write_rowid: 0,
            bdelete: false,
            nrows: 0,
            totals: None,
            sp,
            epoch,
            #[cfg(feature = "fts5_delete")]
            pstruct: None,
            #[cfg(feature = "fts5_delete")]
            ncdel: 0,
        }
    }

    /// `fts5IndexDiscardData` (and the structure cache dropped).
    fn discard(&mut self) {
        self.hash = Hash::new(self.hash.detail());
        self.nrows = 0;
        #[cfg(feature = "fts5_delete")]
        {
            self.pstruct = None;
            self.ncdel = 0;
        }
    }

    /// Whether a flush has anything to write (`nPendingData ||
    /// nContentlessDelete`).
    fn dirty(&self) -> bool {
        #[cfg(feature = "fts5_delete")]
        if self.ncdel > 0 {
            return true;
        }
        !self.hash.is_empty()
    }

    /// `st`'s cached structure in place of the stored one.
    #[cfg(feature = "fts5_delete")]
    fn overlay(&self, store: &mut Store) {
        if let Some(b) = &self.pstruct {
            store.load(crate::fts5::STRUCTURE_ROWID, b.clone());
        }
    }
}

/// The shadow tables of one fts5 table.
#[derive(Clone)]
struct Tab {
    name: AVec<u8>,
    cfg: Config,
    data: u32,
    idx: u32,
    content: Option<u32>,
    docsize: Option<u32>,
    config: u32,
}

/// The rows of one planned write (`FtWrite::kind`).
#[derive(Clone)]
pub(super) enum FtKind {
    /// INSERT: where each value goes and the rows.
    Insert {
        targets: AVec<Target>,
        rows: AVec<AVec<RExpr>>,
        select: Option<Box<crate::query::SubPlan>>,
    },
    /// UPDATE (`sets`) or DELETE: the rows of `SELECT *, rowid,
    /// <sets> FROM t WHERE ...`.
    Change {
        select: Box<crate::query::SubPlan>,
        sets: Option<AVec<Target>>,
    },
}

/// Where an INSERT value or UPDATE assignment goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Col(usize),
    Rowid,
    /// The hidden table-name column (a special command).
    Cmd,
    Rank,
}

/// A planned INSERT / UPDATE / DELETE of an fts5 table.
pub(super) struct FtWrite {
    pub conflict: ConflictAction,
    name: AVec<u8>,
    tab: VTab,
    kind: FtKind,
    /// Several rows: the pin opens a statement savepoint for it.
    multi: bool,
    /// An INSERT's RETURNING list (v0.16 review fix: refused), over
    /// the new row: its columns, the hidden table and rank columns
    /// (NULL) and the rowid given (-1 when none, as the pin
    /// evaluates it before xUpdate assigns one).
    pub returning: AVec<RExpr>,
}

fn quote_sq(name: &[u8], out: &mut AVec<u8>) {
    out.push(b'\'');
    for b in name.iter() {
        if *b == b'\'' {
            out.push(b'\'');
        }
        out.push(*b);
    }
    out.push(b'\'');
}

fn quote_dq(name: &[u8], out: &mut AVec<u8>) {
    out.push(b'"');
    for b in name.iter() {
        if *b == b'"' {
            out.push(b'"');
        }
        out.push(*b);
    }
    out.push(b'"');
}

fn binary_orders(n: usize) -> AVec<KeyOrder> {
    let mut o = AVec::new();
    for _ in 0..n {
        o.push(KeyOrder::default());
    }
    o
}

/// `sqlite3Fts5IndexCharlenToBytelen`: the bytes of the first `n`
/// characters of `p` (0 when it has fewer).
fn charlen_bytes(p: &[u8], nchar: usize) -> usize {
    let mut n = 0usize;
    for i in 0..nchar {
        if n >= p.len() {
            return 0;
        }
        let b = p[n];
        n += 1;
        if b >= 0xC0 {
            if n >= p.len() {
                return 0;
            }
            while p.get(n).is_some_and(|c| c & 0xC0 == 0x80) {
                n += 1;
                if n >= p.len() {
                    if i + 1 == nchar {
                        break;
                    }
                    return 0;
                }
            }
        }
    }
    n
}

/// The averages record.
fn encode_totals(t: &(i64, AVec<i64>)) -> AVec<u8> {
    let mut b = AVec::new();
    crate::fts5::pv(&mut b, t.0 as u64);
    for x in t.1.iter() {
        crate::fts5::pv(&mut b, *x as u64);
    }
    b
}

fn decode_totals(b: &[u8], ncol: usize) -> (i64, AVec<i64>) {
    let mut i = 0;
    let n = crate::fts5::gv(b, &mut i).unwrap_or(0) as i64;
    let mut v = AVec::new();
    for _ in 0..ncol {
        v.push(crate::fts5::gv(b, &mut i).unwrap_or(0) as i64);
    }
    (n, v)
}

/// Whether `e` is `rowid = <expr>` (the pin's one-pass UPDATE /
/// DELETE, which opens no statement savepoint).
fn rowid_eq(arena: &Arena, e: Option<&Expr>) -> bool {
    let is_rowid = |x: &Expr| match x {
        Expr::Column { name, .. } => {
            let n = handle_bytes(arena, *name);
            eq_ci(n, b"rowid") || eq_ci(n, b"oid") || eq_ci(n, b"_rowid_")
        }
        _ => false,
    };
    // `rowid IN (constant)` is the pin's `rowid = +constant` (its
    // parser rewrites a one-value IN list; v0.17: it was taken for a
    // multi-row write, flushing at a different point).
    let lit = |x: &Expr| match x {
        Expr::Literal(_) => true,
        #[cfg(feature = "params")]
        Expr::Param(..) => true,
        Expr::Neg(a) | Expr::Pos(a) => matches!(**a, Expr::Literal(_)),
        _ => false,
    };
    match e {
        Some(Expr::Binary(BinOp::Eq, l, r)) => is_rowid(l) || is_rowid(r),
        Some(Expr::InList(l, list, false)) => {
            is_rowid(l) && list.len() == 1 && list.iter().all(lit)
        }
        _ => false,
    }
}

/// One row of an fts5 write: the old rowid (a delete), and the new
/// row (rowid, columns, the hidden command column, rank).
type Work = (
    Option<i64>,
    Option<(OwnedValue, AVec<OwnedValue>, OwnedValue, OwnedValue)>,
);

impl<S: Storage> Connection<S> {
    /// Plan an INSERT / UPDATE / DELETE of fts5 table `name`.
    pub(super) fn plan_fts5_write(
        &self,
        arena: &Arena,
        ast: &Ast,
        name: &[u8],
        tab: VTab,
    ) -> Result<Plan, S::Error> {
        use crate::query::{ScopeGroup, has_agg, resolve_scoped};
        let ncol = tab.visible();
        let col_of = |n: &[u8]| -> Result<Target, S::Error> {
            if let Some(i) = tab.cols.iter().position(|c| eq_ci(c.name.as_slice(), n)) {
                return Ok(if i < ncol {
                    Target::Col(i)
                } else if i == ncol {
                    Target::Cmd
                } else {
                    Target::Rank
                });
            }
            if eq_ci(n, b"rowid") || eq_ci(n, b"oid") || eq_ci(n, b"_rowid_") {
                return Ok(Target::Rowid);
            }
            Err(ft_err(SchemaKind::UnknownColumn))
        };
        let mut ret = AVec::new();
        let (conflict, kind, multi) = match ast {
            Ast::Insert {
                columns,
                rows,
                select,
                conflict,
                upsert,
                returning,
                ..
            } => {
                if upsert.is_some() {
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
                        for i in 0..ncol {
                            targets.push(Target::Col(i));
                        }
                    }
                }
                let mut rr = AVec::new();
                let cur = ScopeGroup::new(AVec::new(), AVec::new(), 0);
                let mut planner = self.sub_planner(arena);
                for row in rows.iter() {
                    if row.len() != targets.len() && !(row.is_empty() && columns.is_none()) {
                        return Err(ft_err(SchemaKind::ColumnCountMismatch));
                    }
                    let mut out = AVec::new();
                    for e in row.iter() {
                        let r = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
                        if has_agg(&r) {
                            return Err(ft_err(SchemaKind::MisusedAggregate));
                        }
                        out.push(r);
                    }
                    rr.push(out);
                }
                let sel = match select {
                    Some(s) => {
                        let p = self.plan_subquery(s, arena, &[])?;
                        if p.result_names.len() != targets.len() {
                            return Err(ft_err(SchemaKind::ColumnCountMismatch));
                        }
                        Some(Box::new(p))
                    }
                    None => None,
                };
                let multi = rr.len() > 1 || sel.is_some();
                if !returning.is_empty() {
                    let mut cols = AVec::new();
                    for c in tab.cols.iter() {
                        cols.push((c.name.clone(), c.affinity()));
                    }
                    let mut src = crate::query::SourceDesc::new(copy_vec(name), cols, None, true);
                    src.vt_visible = ncol;
                    let mut srcs = AVec::new();
                    srcs.push(src);
                    let mut offs = AVec::new();
                    offs.push(0);
                    ret = self.plan_returning_in(arena, srcs, offs, returning)?;
                }
                (
                    *conflict,
                    FtKind::Insert {
                        targets,
                        rows: rr,
                        select: sel,
                    },
                    multi,
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
                if !returning.is_empty() {
                    return Err(Error::Unsupported);
                }
                let mut sets = AVec::new();
                let mut extra = AVec::new();
                extra.push(rowid_expr(arena)?);
                for (c, e) in assignments.iter() {
                    sets.push(col_of(handle_bytes(arena, *c))?);
                    extra.push(e.clone());
                }
                let multi = !from.is_empty() || !rowid_eq(arena, filter.as_ref());
                let sel = self.ft_change_select(arena, ast, *alias, filter.clone(), extra)?;
                (
                    *conflict,
                    FtKind::Change {
                        select: Box::new(sel),
                        sets: Some(sets),
                    },
                    multi,
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
                let multi = !rowid_eq(arena, filter.as_ref());
                let mut sel = self.ft_change_select(arena, ast, None, filter.clone(), extra)?;
                // A DELETE reads only the rowid (the pin's xUpdate gets
                // no old values).
                if let Some(v) = sel
                    .sources
                    .as_mut_slice()
                    .first_mut()
                    .and_then(|s| s.vt.as_deref_mut())
                {
                    v.content_used = false;
                }
                (
                    ConflictAction::Abort,
                    FtKind::Change {
                        select: Box::new(sel),
                        sets: None,
                    },
                    multi,
                )
            }
            _ => return Err(Error::Unsupported),
        };
        let conflict = if conflict == ConflictAction::Unset {
            ConflictAction::Abort
        } else {
            conflict
        };
        Ok(Plan::Vtab(Box::new(VtPlan::Fts5Write(Box::new(FtWrite {
            conflict,
            name: copy_vec(name),
            tab,
            kind,
            multi,
            returning: ret,
        })))))
    }

    fn ft_change_select(
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
        let mut sel = crate::sql::parser::star_select(*table, alias, filter, extra, arena);
        // UPDATE ... FROM (v0.16 review fix: refused): the table's own
        // columns (`t.*`) and rowid, the FROM sources joined first (the
        // pin's plan probes the table per joined row, so rows are
        // written in the join's order).
        if let Ast::Update { from, .. } = ast
            && !from.is_empty()
        {
            let q = alias.unwrap_or(*table);
            if let Some(it) = sel.items.as_mut_slice().first_mut() {
                *it = SelectItem::TableStar(q);
            }
            if let Some(SelectItem::Expr(Expr::Column { table: t, .. }, ..)) =
                sel.items.as_mut_slice().get_mut(1)
            {
                *t = Some(q);
            }
            let mut srcs = from.clone();
            for mut f in core::mem::take(&mut sel.from).into_iter() {
                f.kind = crate::sql::JoinKind::Inner;
                srcs.push(f);
            }
            sel.from = srcs;
        }
        self.plan_subquery(&sel, arena, &[])
    }

    /// The shadow tables of fts5 table `name`.
    fn ft_tab(&self, name: &[u8], cfg: &Config) -> Result<Tab, S::Error> {
        let ca = &self.catalog_arena;
        let root = |sfx: &[u8]| -> Option<u32> {
            let mut n = copy_vec(name);
            n.extend_from_slice(sfx);
            self.catalog.find(ca, n.as_slice()).map(|t| t.root)
        };
        let content = if cfg.has_content() {
            Some(root(b"_content").ok_or(corrupt())?)
        } else {
            None
        };
        let docsize = if cfg.columnsize {
            Some(root(b"_docsize").ok_or(corrupt())?)
        } else {
            None
        };
        Ok(Tab {
            name: copy_vec(name),
            cfg: cfg.clone(),
            data: root(b"_data").ok_or(corrupt())?,
            idx: root(b"_idx").ok_or(corrupt())?,
            content,
            docsize,
            config: root(b"_config").ok_or(corrupt())?,
        })
    }

    /// Take table `name`'s transaction state out of the connection
    /// (a fresh one at savepoint depth `sp` when it has none, or a
    /// stale one).
    fn ft_take(&mut self, name: &[u8], detail: Detail, sp: usize) -> FtState {
        let epoch = self.pager.txn_gen;
        if let Some(i) = self.fts.iter().position(|s| eq_ci(s.name.as_slice(), name))
            && let Some(s) = self.fts.remove_at(i)
            && s.epoch == epoch
        {
            return s;
        }
        let mut s = FtState::new(copy_vec(name), detail, sp, epoch);
        if let Some(i) = self
            .fts_rows
            .iter()
            .position(|r| eq_ci(r.0.as_slice(), name))
            && let Some(r) = self.fts_rows.remove_at(i)
        {
            s.nrows = r.1;
        }
        s
    }

    /// A synced state leaves the connection: the rows it counted and
    /// did not flush stay counted (`nPendingRow`).
    fn ft_leave(&mut self, st: &FtState) {
        if st.nrows > 0 {
            self.fts_rows.push((st.name.clone(), st.nrows));
        }
    }

    /// Every `%_data` row (the structure as the table's transaction
    /// state caches it, when it has changed one).
    async fn ft_store(&mut self, t: &Tab) -> Result<Store, S::Error> {
        let mut st = Store::default();
        let mut cur = ScanCursor::new(t.data);
        let mut buf = AVec::new();
        while let Some(id) = cur.next_row_into(&mut self.pager, &mut buf).await? {
            let b = match field::<S::Error>(buf.as_slice(), 1)? {
                OwnedValue::Blob(b) | OwnedValue::Text(b) => b,
                _ => AVec::new(),
            };
            st.load(id, b);
        }
        #[cfg(feature = "fts5_delete")]
        if let Some(p) = self.ft_pending(t.name.as_slice()) {
            p.overlay(&mut st);
        }
        Ok(st)
    }

    /// Row `id` of `%_data` into `store`, when there is one.
    #[cfg(feature = "fts5_delete")]
    async fn ft_load_row(&mut self, t: &Tab, store: &mut Store, id: i64) -> Result<(), S::Error> {
        if let Some(p) = btree::seek_rowid(&mut self.pager, t.data, id).await? {
            let b = match field::<S::Error>(p.as_slice(), 1)? {
                OwnedValue::Blob(b) | OwnedValue::Text(b) => b,
                _ => AVec::new(),
            };
            store.load(id, b);
        }
        Ok(())
    }

    /// Replace (or insert) row `k` of the rowid table at `root`.
    async fn ft_put(&mut self, root: u32, k: i64, vals: &[Value<'_>]) -> Result<(), S::Error> {
        let mut rec = AVec::new();
        encode_record(vals, &mut rec);
        btree::btree_delete(&mut self.pager, root, k).await?;
        btree::btree_insert(&mut self.pager, root, k, rec.as_slice()).await
    }

    /// Apply an index operation's writes, in order.
    async fn ft_apply(&mut self, t: &Tab, log: &[FtOp]) -> Result<(), S::Error> {
        let orders = binary_orders(2);
        for op in log.iter() {
            match op {
                FtOp::Put(id, b) => {
                    self.ft_put(t.data, *id, &[Value::Null, Value::Blob(b.as_slice())])
                        .await?
                }
                FtOp::Del(lo, hi) => {
                    let mut gone = AVec::new();
                    let mut cur = ScanCursor::new(t.data);
                    let mut buf = AVec::new();
                    while let Some(id) = cur.next_row_into(&mut self.pager, &mut buf).await? {
                        if id >= *lo && id <= *hi {
                            gone.push(id);
                        }
                    }
                    for id in gone.iter() {
                        btree::btree_delete(&mut self.pager, t.data, *id).await?;
                    }
                }
                FtOp::IdxPut(seg, term, pg) => {
                    let mut rec = AVec::new();
                    encode_record(
                        &[
                            Value::Integer(i64::from(*seg)),
                            Value::Blob(term.as_slice()),
                            Value::Integer(*pg),
                        ],
                        &mut rec,
                    );
                    btree::index_insert_core(
                        &mut self.pager,
                        t.idx,
                        rec.as_slice(),
                        orders.as_slice(),
                    )
                    .await?;
                }
                #[cfg(feature = "fts5_delete")]
                FtOp::IdxDelPg(seg, pg) => {
                    let mut probe = AVec::new();
                    probe.push(btree::KeyPart::Val(OwnedValue::Integer(i64::from(*seg))));
                    probe.push(btree::KeyPart::Min);
                    let (mut cur, _) = btree::IndexCursor::seek_found(
                        &mut self.pager,
                        t.idx,
                        orders.as_slice(),
                        probe.as_slice(),
                    )
                    .await?;
                    let mut gone: AVec<AVec<u8>> = AVec::new();
                    while let Some(e) = cur.next_entry(&mut self.pager).await? {
                        let v = e.values.as_slice();
                        if !matches!(v.first(), Some(OwnedValue::Integer(s)) if *s == i64::from(*seg))
                        {
                            break;
                        }
                        if !matches!(v.get(2), Some(OwnedValue::Integer(p)) if *p >> 1 == i64::from(*pg))
                        {
                            continue;
                        }
                        let vals: AVec<Value<'_>> = v.iter().map(|x| x.as_value()).collect();
                        let mut rec = AVec::new();
                        encode_record(vals.as_slice(), &mut rec);
                        gone.push(rec);
                    }
                    for rec in gone.iter() {
                        btree::index_delete_core(
                            &mut self.pager,
                            t.idx,
                            rec.as_slice(),
                            orders.as_slice(),
                        )
                        .await?;
                    }
                }
                FtOp::IdxDel(seg) => {
                    let mut probe = AVec::new();
                    probe.push(btree::KeyPart::Val(OwnedValue::Integer(i64::from(*seg))));
                    probe.push(btree::KeyPart::Min);
                    let (mut cur, _) = btree::IndexCursor::seek_found(
                        &mut self.pager,
                        t.idx,
                        orders.as_slice(),
                        probe.as_slice(),
                    )
                    .await?;
                    let mut gone: AVec<AVec<u8>> = AVec::new();
                    while let Some(e) = cur.next_entry(&mut self.pager).await? {
                        if !matches!(e.values.as_slice().first(), Some(OwnedValue::Integer(s)) if *s == i64::from(*seg))
                        {
                            break;
                        }
                        let mut vals = AVec::new();
                        for v in e.values.iter() {
                            vals.push(v.as_value());
                        }
                        let mut rec = AVec::new();
                        encode_record(vals.as_slice(), &mut rec);
                        gone.push(rec);
                    }
                    for rec in gone.iter() {
                        btree::index_delete_core(
                            &mut self.pager,
                            t.idx,
                            rec.as_slice(),
                            orders.as_slice(),
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The `%_config` settings (`sqlite3Fts5ConfigLoad`).
    async fn ft_runtime(&mut self, t: &Tab) -> Result<Runtime, S::Error> {
        let mut rt = Runtime::new();
        rt.t.cdel = t.cfg.contentless_delete;
        let orders = binary_orders(1);
        let mut cur = btree::IndexCursor::with_fields(t.config, orders.as_slice(), 0);
        #[cfg(feature = "fts5_delete")]
        let mut sd_row = false;
        while let Some(e) = cur.next_entry(&mut self.pager).await? {
            let (Some(k), v) = (e.values.as_slice().first(), e.values.as_slice().get(1)) else {
                continue;
            };
            let key = match k {
                OwnedValue::Text(t) | OwnedValue::Blob(t) => t.clone(),
                _ => continue,
            };
            let v = v.cloned().unwrap_or(OwnedValue::Null);
            #[cfg(feature = "fts5_delete")]
            {
                sd_row |= eq_ci(key.as_slice(), b"secure-delete");
            }
            if eq_ci(key.as_slice(), b"version") {
                rt.version = crate::func::int64_value(&v);
            } else {
                let _ = rt.set(key.as_slice(), &v);
            }
        }
        if rt.version != 4 && rt.version != 5 {
            return Err(ft_err(SchemaKind::Fts5Definition));
        }
        #[cfg(feature = "fts5_delete")]
        {
            // The pin keeps the `version` it loaded until the structure
            // cookie moves: a `delete-all` writes 4 without moving it,
            // and the connection goes on believing 5.
            let name = t.name.as_slice();
            match self.fts_ver.iter().find(|v| eq_ci(v.0.as_slice(), name)) {
                Some(v) => rt.version = v.1,
                None => self.fts_ver.push((t.name.clone(), rt.version)),
            }
            // `bSecureDelete` has no default a reload restores: with no
            // `secure-delete` row (a rolled-back setting) it keeps the
            // value the connection had.
            match self
                .fts_sd
                .as_mut_slice()
                .iter_mut()
                .find(|v| eq_ci(v.0.as_slice(), name))
            {
                Some(v) if !sd_row => rt.secure_delete = v.1,
                Some(v) => v.1 = rt.secure_delete,
                None => self.fts_sd.push((t.name.clone(), rt.secure_delete)),
            }
        }
        Ok(rt)
    }

    /// The connection now holds `version` / `secure-delete` (when
    /// given) for table `t`.
    #[cfg(feature = "fts5_delete")]
    fn ft_hold(&mut self, t: &Tab, ver: Option<i64>, sd: Option<bool>) {
        let name = t.name.as_slice();
        if let (Some(x), Some(v)) = (
            ver,
            self.fts_ver
                .as_mut_slice()
                .iter_mut()
                .find(|v| eq_ci(v.0.as_slice(), name)),
        ) {
            v.1 = x;
        }
        if let (Some(x), Some(v)) = (
            sd,
            self.fts_sd
                .as_mut_slice()
                .iter_mut()
                .find(|v| eq_ci(v.0.as_slice(), name)),
        ) {
            v.1 = x;
        }
    }

    /// `REPLACE INTO %_config VALUES(k, v)`.
    async fn ft_config_put(&mut self, t: &Tab, k: &[u8], v: &OwnedValue) -> Result<(), S::Error> {
        let orders = binary_orders(1);
        let mut probe = AVec::new();
        probe.push(btree::KeyPart::Val(OwnedValue::Text(copy_vec(k))));
        let (mut cur, found) = btree::IndexCursor::seek_found(
            &mut self.pager,
            t.config,
            orders.as_slice(),
            probe.as_slice(),
        )
        .await?;
        if found && let Some(e) = cur.next_entry(&mut self.pager).await? {
            let mut vals = AVec::new();
            for x in e.values.iter() {
                vals.push(x.as_value());
            }
            let mut rec = AVec::new();
            encode_record(vals.as_slice(), &mut rec);
            btree::index_delete_core(&mut self.pager, t.config, rec.as_slice(), orders.as_slice())
                .await?;
        }
        let mut rec = AVec::new();
        encode_record(&[Value::Text(k), v.as_value()], &mut rec);
        btree::index_insert_core(&mut self.pager, t.config, rec.as_slice(), orders.as_slice()).await
    }

    /// Load the totals into `st` (`fts5StorageLoadTotals`).
    async fn ft_totals(&mut self, t: &Tab, st: &mut FtState) -> Result<(), S::Error> {
        if st.totals.is_none() {
            let ncol = t.cfg.cols.len();
            let b = match btree::seek_rowid(&mut self.pager, t.data, crate::fts5::AVERAGES_ROWID)
                .await?
            {
                Some(p) => match field::<S::Error>(p.as_slice(), 1)? {
                    OwnedValue::Blob(b) | OwnedValue::Text(b) => b,
                    _ => AVec::new(),
                },
                None => AVec::new(),
            };
            st.totals = Some(decode_totals(b.as_slice(), ncol));
        }
        Ok(())
    }

    /// The pending terms into a new segment (`fts5IndexFlush`).
    async fn ft_flush_index(
        &mut self,
        t: &Tab,
        rt: &Runtime,
        st: &mut FtState,
    ) -> Result<(), S::Error> {
        if !st.dirty() {
            return Ok(());
        }
        let mut store = self.ft_store(t).await?;
        #[cfg(feature = "fts5_delete")]
        let mut x = {
            st.overlay(&mut store);
            let mut x = crate::fts5::sdel::SdFlush {
                ncdel: st.ncdel,
                secure: rt.secure_delete,
                ..Default::default()
            };
            if x.secure {
                // `iVersion` as the connection holds it (an earlier
                // flush of this statement may have moved it).
                x.v5 = self.ft_runtime(t).await?.version == 5;
            }
            x
        };
        crate::fts5::flush(
            &mut store,
            &st.hash,
            &rt.t,
            t.cfg.detail,
            st.nrows,
            #[cfg(feature = "fts5_delete")]
            &mut x,
        )
        .map_err(|_| corrupt())?;
        self.ft_apply(t, store.log.as_slice()).await?;
        #[cfg(feature = "fts5_delete")]
        if x.bumped {
            // fts5FlushSecureDelete: `REPLACE INTO %_config
            // VALUES('version', 5)` (the cookie moved on with it).
            self.ft_config_put(t, b"version", &OwnedValue::Integer(5))
                .await?;
            self.ft_hold(t, Some(5), None);
        }
        st.discard();
        Ok(())
    }

    /// `sqlite3Fts5StorageSync`: the totals, then the pending terms.
    async fn ft_sync(&mut self, t: &Tab, rt: &Runtime, st: &mut FtState) -> Result<(), S::Error> {
        if let Some(tot) = st.totals.take() {
            let b = encode_totals(&tot);
            self.ft_put(
                t.data,
                crate::fts5::AVERAGES_ROWID,
                &[Value::Null, Value::Blob(b.as_slice())],
            )
            .await?;
        }
        self.ft_flush_index(t, rt, st).await
    }

    /// The table and settings a state belongs to.
    async fn ft_resolve(&mut self, name: &[u8]) -> Result<Option<(Tab, Runtime)>, S::Error> {
        let ca = &self.catalog_arena;
        let Some(Ok(vt)) = self.catalog.find_vtab(ca, name).map(|v| v.tab.clone()) else {
            return Ok(None);
        };
        let Some(cfg) = vt.fts.as_deref() else {
            return Ok(None);
        };
        let t = self.ft_tab(name, cfg)?;
        let rt = self.ft_runtime(&t).await?;
        Ok(Some((t, rt)))
    }

    /// Flush every table's pending terms and totals (COMMIT, and the
    /// RELEASE that commits).
    pub(super) async fn fts_sync_all(&mut self) -> Result<(), S::Error> {
        if self.fts.is_empty() {
            return Ok(());
        }
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.fts_sync_all_impl()).await
    }

    async fn fts_sync_all_impl(&mut self) -> Result<(), S::Error> {
        #[cfg(feature = "fts5_delete")]
        self.fts_ver.clear();
        let epoch = self.pager.txn_gen;
        let states = core::mem::take(&mut self.fts);
        for mut st in states.into_iter() {
            if st.epoch != epoch {
                continue;
            }
            if let Some((t, rt)) = self.ft_resolve(st.name.clone().as_slice()).await? {
                self.ft_sync(&t, &rt, &mut st).await?;
                self.ft_leave(&st);
            }
        }
        Ok(())
    }

    /// `xSavepoint(level)` for every table in the transaction.
    pub(super) async fn fts_savepoint(&mut self, level: usize) -> Result<(), S::Error> {
        if self.fts.is_empty() {
            return Ok(());
        }
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.fts_savepoint_impl(level)).await
    }

    async fn fts_savepoint_impl(&mut self, level: usize) -> Result<(), S::Error> {
        let epoch = self.pager.txn_gen;
        let states = core::mem::take(&mut self.fts);
        let mut kept = AVec::new();
        for mut st in states.into_iter() {
            if st.epoch != epoch {
                continue;
            }
            if let Some((t, rt)) = self.ft_resolve(st.name.clone().as_slice()).await? {
                self.ft_sync(&t, &rt, &mut st).await?;
            }
            st.sp = level + 1;
            kept.push(st);
        }
        self.fts = kept;
        Ok(())
    }

    /// `xRelease(level)`.
    pub(super) async fn fts_release(&mut self, level: usize) -> Result<(), S::Error> {
        if self.fts.is_empty() {
            return Ok(());
        }
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.fts_release_impl(level)).await
    }

    async fn fts_release_impl(&mut self, level: usize) -> Result<(), S::Error> {
        let epoch = self.pager.txn_gen;
        let states = core::mem::take(&mut self.fts);
        let mut kept = AVec::new();
        for mut st in states.into_iter() {
            if st.epoch != epoch {
                continue;
            }
            if level + 1 < st.sp {
                if let Some((t, rt)) = self.ft_resolve(st.name.clone().as_slice()).await? {
                    self.ft_sync(&t, &rt, &mut st).await?;
                }
                st.sp = level;
            }
            kept.push(st);
        }
        self.fts = kept;
        Ok(())
    }

    /// `sqlite3Fts5StorageSync` of table `name` alone: its pending
    /// terms flushed and its totals written, the table staying in
    /// the transaction.
    pub(super) async fn fts5_sync_one(
        &mut self,
        name: &[u8],
        cfg: &Config,
    ) -> Result<(), S::Error> {
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.fts5_sync_one_impl(name, cfg)).await
    }

    async fn fts5_sync_one_impl(&mut self, name: &[u8], cfg: &Config) -> Result<(), S::Error> {
        if self.ft_pending(name).is_none() {
            return Ok(());
        }
        let t = self.ft_tab(name, cfg)?;
        let mut st = self.ft_take(name, cfg.detail, 0);
        let r = match self.ft_runtime(&t).await {
            Ok(rt) => self.ft_sync(&t, &rt, &mut st).await,
            Err(e) => Err(e),
        };
        self.fts.push(st);
        r
    }

    /// Whether a trigger or foreign-key program is running.
    pub(super) fn fts_in_program(&self) -> bool {
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        let p = self.in_program();
        #[cfg(not(any(feature = "triggers", feature = "foreign_keys")))]
        let p = false;
        p
    }

    /// The pin reparsed its schema (`sqlite3ResetAllSchemasOfConnection`
    /// after ALTER TABLE, VACUUM, a rollback that undid a schema change
    /// or `PRAGMA writable_schema = RESET`): every fts5 table's
    /// `Fts5Config` is built afresh, so a rolled-back `secure-delete`
    /// setting the connection kept is gone (v0.17 review fix: hipp
    /// kept it and went on doing secure deletes).
    #[cfg(feature = "fts5_delete")]
    pub(super) fn fts5_schema_reset(&mut self) {
        self.fts_sd.clear();
    }

    /// Table `old` was renamed `new`: its pending terms follow it.
    pub(super) fn fts5_renamed(&mut self, old: &[u8], new: &[u8]) {
        for st in self.fts.as_mut_slice().iter_mut() {
            if eq_ci(st.name.as_slice(), old) {
                st.name = copy_vec(new);
            }
        }
        for r in self.fts_rows.as_mut_slice().iter_mut() {
            if eq_ci(r.0.as_slice(), old) {
                r.0 = copy_vec(new);
            }
        }
        #[cfg(feature = "fts5_delete")]
        for r in self.fts_sd.as_mut_slice().iter_mut() {
            if eq_ci(r.0.as_slice(), old) {
                r.0 = copy_vec(new);
            }
        }
    }

    /// The pending terms of table `name` in this transaction, if any.
    fn ft_pending(&self, name: &[u8]) -> Option<&FtState> {
        let epoch = self.pager.txn_gen;
        self.fts
            .iter()
            .find(|s| eq_ci(s.name.as_slice(), name) && s.epoch == epoch)
    }
}

/// `rowid op v` as the pin compares it (the rowid's INTEGER affinity
/// applied to `v`; NULL is never true, text and blobs sort above
/// numbers).
fn rowid_cmp(r: i64, v: &OwnedValue, op: crate::vtab::Op) -> bool {
    use crate::vtab::Op;
    use core::cmp::Ordering;
    let n = match v {
        OwnedValue::Text(t) => crate::record::text_to_number(t.as_slice()).unwrap_or(v.clone()),
        x => x.clone(),
    };
    let o = match n {
        OwnedValue::Null => return false,
        OwnedValue::Integer(i) => r.cmp(&i),
        OwnedValue::Real(f) => {
            if crate::record::is_integral_real(f) {
                r.cmp(&(f as i64))
            } else {
                (r as f64).partial_cmp(&f).unwrap_or(Ordering::Less)
            }
        }
        _ => Ordering::Less,
    };
    match op {
        Op::Eq => o.is_eq(),
        Op::Lt => o.is_lt(),
        Op::Le => o.is_le(),
        Op::Gt => o.is_gt(),
        _ => o.is_ge(),
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

/// The field `i` of a record as an owned value (NULL past its end).
fn field<E>(payload: &[u8], i: usize) -> Result<OwnedValue, E> {
    let vals = crate::record::decode_record::<E>(payload, i + 1)?;
    Ok(vals
        .as_slice()
        .get(i)
        .map_or(OwnedValue::Null, |v| OwnedValue::from_value(*v)))
}

impl<S: Storage> Statement<'_, S> {
    /// Whether the pin reparses its schema after this statement
    /// (`ok`: it succeeded; `was_txn`: an explicit transaction was
    /// open when it started; `ck0`: the schema cookie then): a
    /// successful ALTER TABLE or VACUUM (INTO too), or a rollback —
    /// ROLLBACK, ROLLBACK TO, a failure that ended the transaction —
    /// that moved the cookie back.
    #[cfg(feature = "fts5_delete")]
    pub(super) fn reloads_schema(&self, ok: bool, was_txn: bool, ck0: u32) -> bool {
        use super::maint::Maint;
        let back = self.conn.pager.schema_cookie() != ck0;
        match &self.plan {
            Plan::Rollback | Plan::Maint(Maint::RollbackTo(_)) => ok && back,
            Plan::AlterAdd { .. } | Plan::Alter16 { .. } | Plan::Maint(Maint::Vacuum { .. }) => ok,
            Plan::Vtab(v) if matches!(**v, VtPlan::Rename { .. }) => ok,
            _ => !ok && was_txn && !self.conn.explicit_txn && back,
        }
    }

    // -----------------------------------------------------------
    // Shadow-table access
    // -----------------------------------------------------------

    /// `xRollbackTo(level)`: the pending terms and totals go.
    pub(super) fn fts_rollback_to(&mut self, level: usize) {
        for st in self.conn.fts.as_mut_slice().iter_mut() {
            if level < st.sp {
                st.discard();
                st.totals = None;
            }
        }
    }

    // -----------------------------------------------------------
    // CREATE / DROP
    // -----------------------------------------------------------

    /// One shadow table: its schema row and empty root.
    async fn ft_shadow(
        &mut self,
        name: &[u8],
        sfx: &[u8],
        cols: &[u8],
        without: bool,
    ) -> Result<u32, S::Error> {
        let mut tname = copy_vec(name);
        tname.extend_from_slice(sfx);
        let mut sql = copy_vec(b"CREATE TABLE ");
        quote_sq(tname.as_slice(), &mut sql);
        sql.push(b'(');
        sql.extend_from_slice(cols);
        sql.push(b')');
        if without {
            sql.extend_from_slice(b" WITHOUT ROWID");
        }
        let root = alloc_root!(&mut self.conn.pager);
        if without {
            btree::create_empty_index_root(&mut self.conn.pager, root).await?;
        } else {
            btree::create_empty_root(&mut self.conn.pager, root).await?;
        }
        self.insert_schema_row(
            b"table",
            tname.as_slice(),
            tname.as_slice(),
            root,
            Some(sql.as_slice()),
        )
        .await?;
        Ok(root)
    }

    /// xCreate: the shadow tables in the pin's order, the empty
    /// averages and structure records and the `version` row.
    pub(super) async fn fts5_create(&mut self, name: &[u8], tab: &VTab) -> Result<(), S::Error> {
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.fts5_create_impl(name, tab)).await
    }

    async fn fts5_create_impl(&mut self, name: &[u8], tab: &VTab) -> Result<(), S::Error> {
        let Some(cfg) = tab.fts.as_deref() else {
            return Ok(());
        };
        let cfg = cfg.clone();
        let data = self
            .ft_shadow(name, b"_data", b"id INTEGER PRIMARY KEY, block BLOB", false)
            .await?;
        self.ft_shadow(
            name,
            b"_idx",
            b"segid, term, pgno, PRIMARY KEY(segid, term)",
            true,
        )
        .await?;
        let mut s = crate::fts5::Structure::empty(cfg.contentless_delete).encode();
        self.conn
            .ft_put(
                data,
                crate::fts5::AVERAGES_ROWID,
                &[Value::Null, Value::Blob(&[])],
            )
            .await?;
        self.conn
            .ft_put(
                data,
                crate::fts5::STRUCTURE_ROWID,
                &[Value::Null, Value::Blob(s.as_slice())],
            )
            .await?;
        s.clear();
        if cfg.has_content() {
            let mut cols = copy_vec(b"id INTEGER PRIMARY KEY");
            for i in 0..cfg.cols.len() {
                cols.extend_from_slice(b", c");
                crate::record::render_int(i as i64, &mut cols);
            }
            self.ft_shadow(name, b"_content", cols.as_slice(), false)
                .await?;
        }
        if cfg.columnsize {
            let cols: &[u8] = if cfg.contentless_delete {
                b"id INTEGER PRIMARY KEY, sz BLOB, origin INTEGER"
            } else {
                b"id INTEGER PRIMARY KEY, sz BLOB"
            };
            self.ft_shadow(name, b"_docsize", cols, false).await?;
        }
        let config = self
            .ft_shadow(name, b"_config", b"k PRIMARY KEY, v", true)
            .await?;
        let orders = binary_orders(1);
        let mut rec = AVec::new();
        encode_record(&[Value::Text(b"version"), Value::Integer(4)], &mut rec);
        btree::index_insert_core(
            &mut self.conn.pager,
            config,
            rec.as_slice(),
            orders.as_slice(),
        )
        .await
    }

    /// The shadow tables xDestroy drops, in the pin's order.
    pub(super) fn fts5_shadows(tab: &VTab) -> AVec<&'static [u8]> {
        let mut v: AVec<&'static [u8]> = AVec::new();
        if let Some(cfg) = tab.fts.as_deref() {
            v.push(b"_data");
            v.push(b"_idx");
            v.push(b"_config");
            if cfg.columnsize {
                v.push(b"_docsize");
            }
            if cfg.has_content() {
                v.push(b"_content");
            }
        }
        v
    }

    /// A dropped table's pending terms go with it.
    pub(super) fn fts5_forget(&mut self, name: &[u8]) {
        if let Some(i) = self
            .conn
            .fts
            .iter()
            .position(|s| eq_ci(s.name.as_slice(), name))
        {
            self.conn.fts.remove_at(i);
        }
        if let Some(i) = self
            .conn
            .fts_rows
            .iter()
            .position(|s| eq_ci(s.0.as_slice(), name))
        {
            self.conn.fts_rows.remove_at(i);
        }
        #[cfg(feature = "fts5_delete")]
        {
            self.conn.fts_ver.clear();
            if let Some(i) = self
                .conn
                .fts_sd
                .iter()
                .position(|s| eq_ci(s.0.as_slice(), name))
            {
                self.conn.fts_sd.remove_at(i);
            }
        }
    }

    /// Whether this statement gets the pin's statement journal
    /// (`usesStmtJournal`: a multi-row write whose constraints may
    /// abort), which flushes the fts5 tables' pending terms: an fts5
    /// write of several rows; a write to a table with triggers; a
    /// multi-row INSERT / UPDATE of a table with constraints.
    pub(super) fn fts_journal(&self) -> bool {
        let arena = &self.arena;
        let (table, multi, kind) = match &self.ast {
            Ast::Insert {
                table,
                rows,
                select,
                ..
            } => (*table, rows.len() > 1 || select.is_some(), 0),
            Ast::Update {
                table,
                filter,
                from,
                ..
            } => (
                *table,
                !from.is_empty() || !rowid_eq(arena, filter.as_ref()),
                1,
            ),
            Ast::Delete { table, filter, .. } => (*table, !rowid_eq(arena, filter.as_ref()), 2),
            _ => return false,
        };
        let name = handle_bytes(arena, table);
        let ca = &self.conn.catalog_arena;
        if self
            .conn
            .catalog
            .find_vtab(ca, name)
            .is_some_and(|v| v.tab.as_ref().is_ok_and(|t| t.fts.is_some()))
        {
            return multi;
        }
        #[cfg(feature = "triggers")]
        if self.conn.catalog.triggers_on(ca, name).next().is_some() {
            return true;
        }
        let Some(t) = self.conn.catalog.find(ca, name) else {
            return false;
        };
        let may_abort = t.rowid_alias.is_some()
            || t.columns.iter().any(|c| c.not_null)
            || !t.checks.is_empty()
            || self.conn.catalog.indexes_for(ca, name).next().is_some();
        multi && may_abort && kind != 2
    }

    // -----------------------------------------------------------
    // Content
    // -----------------------------------------------------------

    /// Run SQL text `sql` (a SELECT) and return its rows.
    async fn ft_sql(&mut self, sql: &[u8]) -> Result<AVec<AVec<OwnedValue>>, S::Error> {
        let arena = Arena::new(crate::error::Scope::Statement, 0, 1 << 20);
        let ast = crate::sql::parse::<S::Error>(sql, &arena, self.conn.cfg.limits())?;
        let Ast::Select(sel) = ast else {
            return Err(Error::Unsupported);
        };
        let plan = self.conn.plan_subquery(&sel, &arena, &[])?;
        self.exec_subplan(&plan, &RowData::empty()).await
    }

    /// `SELECT <rowid>, <cols> FROM <external content>` (`WHERE
    /// <rowid> = r` when given).
    fn ext_sql(t: &Tab, r: Option<i64>) -> AVec<u8> {
        let mut q = copy_vec(b"SELECT ");
        quote_dq(t.cfg.content_rowid.as_slice(), &mut q);
        for c in t.cfg.cols.iter() {
            q.extend_from_slice(b", ");
            quote_dq(c.as_slice(), &mut q);
        }
        q.extend_from_slice(b" FROM ");
        quote_dq(t.cfg.content_tab.as_slice(), &mut q);
        if let Some(r) = r {
            q.extend_from_slice(b" WHERE ");
            quote_dq(t.cfg.content_rowid.as_slice(), &mut q);
            q.extend_from_slice(b" = ");
            crate::record::render_int(r, &mut q);
        }
        q
    }

    /// Row `r`'s column values (`FTS5_STMT_LOOKUP`), `None` when it
    /// has none.
    async fn ft_lookup(&mut self, t: &Tab, r: i64) -> Result<Option<AVec<OwnedValue>>, S::Error> {
        let ncol = t.cfg.cols.len();
        match t.cfg.content {
            Content::Normal => {
                let root = t.content.ok_or(corrupt())?;
                let Some(p) = btree::seek_rowid(&mut self.conn.pager, root, r).await? else {
                    return Ok(None);
                };
                let vals = crate::record::decode_record::<S::Error>(p.as_slice(), ncol + 1)?;
                let mut out = AVec::new();
                for i in 0..ncol {
                    out.push(
                        vals.as_slice()
                            .get(i + 1)
                            .map_or(OwnedValue::Null, |v| OwnedValue::from_value(*v)),
                    );
                }
                Ok(Some(out))
            }
            Content::External => {
                let q = Self::ext_sql(t, Some(r));
                let rows = self.ft_sql(q.as_slice()).await?;
                Ok(rows.as_slice().first().map(|row| {
                    let mut out = AVec::new();
                    for v in row.iter().skip(1) {
                        out.push(v.clone());
                    }
                    out
                }))
            }
            Content::None => Ok(None),
        }
    }

    // -----------------------------------------------------------
    // Index writes
    // -----------------------------------------------------------

    /// `sqlite3Fts5IndexBeginWrite`.
    async fn ft_begin(
        &mut self,
        t: &Tab,
        rt: &Runtime,
        st: &mut FtState,
        del: bool,
        rowid: i64,
    ) -> Result<(), S::Error> {
        if rowid < st.write_rowid
            || (rowid == st.write_rowid && !st.bdelete)
            || st.hash.bytes > rt.t.hashsize
        {
            self.conn.ft_flush_index(t, rt, st).await?;
        }
        st.write_rowid = rowid;
        st.bdelete = del;
        if !del {
            st.nrows += 1;
        }
        Ok(())
    }

    /// Tokenize `vals` into the pending terms (a delete when `del`):
    /// returns each column's token count.
    fn ft_tokens(
        cfg: &Config,
        st: &mut FtState,
        rowid: i64,
        vals: &[OwnedValue],
        del: bool,
    ) -> AVec<i64> {
        let mut sizes = AVec::new();
        for (c, un) in cfg.unindexed.iter().enumerate() {
            let mut sz = 0i64;
            if !*un && let Some(text) = vals.get(c).and_then(crate::fts5::aux::text_of) {
                let col = if del { -1 } else { c as i32 };
                let hash = &mut st.hash;
                cfg.tok.tokenize(text.as_slice(), &mut |tok, _, _| {
                    sz += 1;
                    let tok = &tok[..tok.len().min(32768)];
                    let pos = (sz - 1) as i32;
                    hash.write(rowid, col, pos, b'0', tok);
                    for (i, n) in cfg.prefix.iter().enumerate() {
                        let nb = charlen_bytes(tok, usize::from(*n));
                        if nb > 0 {
                            hash.write(rowid, col, pos, b'1' + i as u8, &tok[..nb]);
                        }
                    }
                });
            }
            sizes.push(sz);
        }
        sizes
    }

    /// `sqlite3Fts5StorageIndexInsert`: tokens, totals, docsize.
    async fn ft_index_insert(
        &mut self,
        t: &Tab,
        rt: &Runtime,
        st: &mut FtState,
        rowid: i64,
        vals: &[OwnedValue],
    ) -> Result<(), S::Error> {
        self.conn.ft_totals(t, st).await?;
        self.ft_begin(t, rt, st, false, rowid).await?;
        let sizes = Self::ft_tokens(&t.cfg, st, rowid, vals, false);
        if let Some(tot) = st.totals.as_mut() {
            for (a, s) in tot.1.as_mut_slice().iter_mut().zip(sizes.iter()) {
                *a = a.wrapping_add(*s);
            }
            tot.0 = tot.0.wrapping_add(1);
        }
        if let Some(ds) = t.docsize {
            let mut blob = AVec::new();
            for s in sizes.iter() {
                crate::fts5::pv(&mut blob, *s as u64);
            }
            if t.cfg.contentless_delete {
                let origin = match btree::seek_rowid(
                    &mut self.conn.pager,
                    t.data,
                    crate::fts5::STRUCTURE_ROWID,
                )
                .await?
                {
                    Some(p) => match field::<S::Error>(p.as_slice(), 1)? {
                        OwnedValue::Blob(b) => {
                            crate::fts5::Structure::decode(b.as_slice())
                                .map_err(|_| corrupt())?
                                .origin
                        }
                        _ => return Err(corrupt()),
                    },
                    None => return Err(corrupt()),
                };
                self.conn
                    .ft_put(
                        ds,
                        rowid,
                        &[
                            Value::Null,
                            Value::Blob(blob.as_slice()),
                            Value::Integer(origin as i64),
                        ],
                    )
                    .await?;
            } else {
                self.conn
                    .ft_put(ds, rowid, &[Value::Null, Value::Blob(blob.as_slice())])
                    .await?;
            }
        }
        Ok(())
    }

    /// `sqlite3Fts5StorageDelete(iDel, apVal)`.
    async fn ft_delete(
        &mut self,
        t: &Tab,
        rt: &Runtime,
        st: &mut FtState,
        rowid: i64,
        vals: Option<&[OwnedValue]>,
    ) -> Result<(), S::Error> {
        #[cfg(not(feature = "fts5_delete"))]
        {
            if t.cfg.contentless_delete {
                return Err(Error::Unsupported);
            }
            if rt.secure_delete {
                return Err(ft_err(SchemaKind::Fts5SecureDelete));
            }
        }
        self.conn.ft_totals(t, st).await?;
        self.ft_begin(t, rt, st, true, rowid).await?;
        #[cfg(feature = "fts5_delete")]
        if t.cfg.contentless_delete {
            return self.ft_contentless_delete(t, rt, st, rowid).await;
        }
        let old = match vals {
            Some(v) => Some(v.iter().cloned().collect::<AVec<_>>()),
            None => self.ft_lookup(t, rowid).await?,
        };
        if let Some(old) = old {
            let sizes = Self::ft_tokens(&t.cfg, st, rowid, old.as_slice(), true);
            if let Some(tot) = st.totals.as_mut() {
                for (a, s) in tot.1.as_mut_slice().iter_mut().zip(sizes.iter()) {
                    *a = a.wrapping_sub(*s);
                    if *a < 0 {
                        return Err(corrupt());
                    }
                }
                if tot.0 < 1 {
                    return Err(corrupt());
                }
                tot.0 -= 1;
            }
        }
        if let Some(ds) = t.docsize {
            btree::btree_delete(&mut self.conn.pager, ds, rowid).await?;
        }
        if let Some(c) = t.content {
            btree::btree_delete(&mut self.conn.pager, c, rowid).await?;
        }
        Ok(())
    }

    /// `fts5StorageContentlessDelete` (v0.17, `fts5_delete`): the row's
    /// `%_docsize` entry gives its sizes (taken off the totals) and
    /// origin; a tombstone for it goes into every segment of that
    /// origin; the entry goes.
    #[cfg(feature = "fts5_delete")]
    async fn ft_contentless_delete(
        &mut self,
        t: &Tab,
        rt: &Runtime,
        st: &mut FtState,
        rowid: i64,
    ) -> Result<(), S::Error> {
        let ds = t.docsize.ok_or(corrupt())?;
        let Some(p) = btree::seek_rowid(&mut self.conn.pager, ds, rowid).await? else {
            return Ok(());
        };
        let vals = crate::record::decode_record::<S::Error>(p.as_slice(), 3)?;
        let sz = vals.as_slice().get(1).copied();
        let origin = vals
            .as_slice()
            .get(2)
            .map_or(0, |v| crate::func::int64_value(&OwnedValue::from_value(*v)));
        if let Some(tot) = st.totals.as_mut() {
            tot.0 = tot.0.wrapping_sub(1);
            // fts5StorageDecodeSizeArray: the sizes only when the blob
            // holds exactly one varint per column.
            let blob: &[u8] = match sz {
                Some(Value::Blob(b)) | Some(Value::Text(b)) => b,
                _ => &[],
            };
            let mut cols = AVec::new();
            let mut i = 0usize;
            let mut ok = true;
            for _ in 0..tot.1.len() {
                if i >= blob.len() {
                    ok = false;
                    break;
                }
                cols.push(crate::fts5::gv(blob, &mut i).map_or(0, |v| v as u32 as i32 as i64));
            }
            if ok && i == blob.len() {
                for (a, c) in tot.1.as_mut_slice().iter_mut().zip(cols.iter()) {
                    *a = a.wrapping_sub(*c);
                }
            }
        }
        if origin != 0 {
            // Only the structure and the tombstone pages the delete
            // touches are read (review fix: every `%_data` row was
            // loaded for each row deleted, quadratic in a statement's
            // deletes): first the one page each hash maps the rowid
            // to, and when a hash must be rebuilt, all of its pages.
            let mut all = false;
            let (store, s, n) = loop {
                let mut store = Store::default();
                self.conn
                    .ft_load_row(t, &mut store, crate::fts5::STRUCTURE_ROWID)
                    .await?;
                if let Some(p) = self.conn.ft_pending(t.name.as_slice()) {
                    p.overlay(&mut store);
                }
                st.overlay(&mut store);
                let mut s = store.structure().map_err(|_| corrupt())?;
                let ids = crate::fts5::sdel::tomb_ids(&s, origin as u64, rowid, all);
                for id in ids.iter() {
                    self.conn.ft_load_row(t, &mut store, *id).await?;
                }
                match crate::fts5::sdel::contentless_delete(
                    &mut store,
                    &mut s,
                    origin as u64,
                    rowid,
                    rt.t.pgsz,
                ) {
                    Ok(n) => break (store, s, n),
                    // (A page the first pass did not read.)
                    Err(_) if !all => all = true,
                    Err(_) => return Err(corrupt()),
                }
            };
            if n > 0 {
                self.conn.ft_apply(t, store.log.as_slice()).await?;
                st.ncdel = st.ncdel.wrapping_add(n);
                st.pstruct = Some(s.encode());
            }
        }
        btree::btree_delete(&mut self.conn.pager, ds, rowid).await?;
        Ok(())
    }

    /// `sqlite3Fts5StorageContentInsert`: the new row's rowid.
    async fn ft_content_insert(
        &mut self,
        t: &Tab,
        rowid: &OwnedValue,
        vals: &[OwnedValue],
    ) -> Result<i64, S::Error> {
        if let Some(c) = t.content {
            let r = match rowid {
                OwnedValue::Integer(r) => *r,
                _ => btree::new_rowid(&mut self.conn.pager, c).await?,
            };
            if btree::seek_rowid(&mut self.conn.pager, c, r)
                .await?
                .is_some()
            {
                return Err(ft_err(SchemaKind::UniqueViolation));
            }
            let mut v = AVec::new();
            v.push(Value::Null);
            for x in vals.iter() {
                v.push(x.as_value());
            }
            let mut rec = AVec::new();
            encode_record(v.as_slice(), &mut rec);
            btree::btree_insert(&mut self.conn.pager, c, r, rec.as_slice()).await?;
            return Ok(r);
        }
        match rowid {
            OwnedValue::Integer(r) => Ok(*r),
            _ => {
                // fts5StorageNewRowid: a %_docsize row holds it.
                let Some(ds) = t.docsize else {
                    return Err(ft_err(SchemaKind::DatatypeMismatch));
                };
                let r = btree::new_rowid(&mut self.conn.pager, ds).await?;
                let mut rec = AVec::new();
                encode_record(&[Value::Null, Value::Null], &mut rec);
                btree::btree_insert(&mut self.conn.pager, ds, r, rec.as_slice()).await?;
                Ok(r)
            }
        }
    }

    /// `sqlite3_value_numeric_type()` of a new rowid: an integer, NULL,
    /// or the pin's "datatype mismatch".
    fn ft_rowid_value(v: &OwnedValue) -> Result<OwnedValue, S::Error> {
        match v {
            OwnedValue::Null | OwnedValue::Integer(_) => Ok(v.clone()),
            OwnedValue::Text(b) => match crate::record::text_to_number(b.as_slice()) {
                Some(OwnedValue::Integer(i)) => Ok(OwnedValue::Integer(i)),
                _ => Err(ft_err(SchemaKind::DatatypeMismatch)),
            },
            _ => Err(ft_err(SchemaKind::DatatypeMismatch)),
        }
    }

    /// An INSERT's new rowid: the `OP_MustBeInt` the pin runs before
    /// xUpdate (an integral REAL or numeric text becomes an integer;
    /// v0.16 review fix: `7.0` was refused), then
    /// [`Self::ft_rowid_value`].
    fn ft_insert_rowid(v: &OwnedValue) -> Result<OwnedValue, S::Error> {
        let n = match v {
            OwnedValue::Text(b) => crate::record::text_to_number(b.as_slice()).unwrap_or(v.clone()),
            _ => v.clone(),
        };
        match n {
            OwnedValue::Real(r)
                if crate::record::is_integral_real(r) || r == -9.223372036854776e18 =>
            {
                Ok(OwnedValue::Integer(r as i64))
            }
            _ => Self::ft_rowid_value(&n),
        }
    }

    // -----------------------------------------------------------
    // The write statement
    // -----------------------------------------------------------

    /// Run a planned fts5 INSERT / UPDATE / DELETE.
    pub(super) async fn exec_fts5_write(&mut self) -> Result<(), S::Error> {
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.exec_fts5_write_impl()).await
    }

    async fn exec_fts5_write_impl(&mut self) -> Result<(), S::Error> {
        let Plan::Vtab(p) = &self.plan else {
            return Err(Error::Unsupported);
        };
        let VtPlan::Fts5Write(w) = &**p else {
            return Err(Error::Unsupported);
        };
        let (name, tab, conflict, kind, multi, ret) = (
            w.name.clone(),
            w.tab.clone(),
            w.conflict,
            w.kind.clone(),
            w.multi,
            w.returning.clone(),
        );
        let cfg = tab.fts.as_deref().ok_or(corrupt())?.clone();
        let ncol = cfg.cols.len();
        // An UPDATE's indexed columns: how many it sets and leaves.
        #[cfg(feature = "fts5_delete")]
        let upd = match &kind {
            FtKind::Change {
                sets: Some(sets), ..
            } => {
                let mut u = (0usize, 0usize);
                for (c, un) in cfg.unindexed.iter().enumerate() {
                    if !*un {
                        if sets.iter().any(|t| *t == Target::Col(c)) {
                            u.0 += 1;
                        } else {
                            u.1 += 1;
                        }
                    }
                }
                u
            }
            _ => (0, 0),
        };
        // (old rowid, new row: rowid, columns, command, rank).
        let mut work: AVec<Work> = AVec::new();
        let empty = RowData::empty();
        match kind {
            FtKind::Insert {
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
                    let mut cols: AVec<OwnedValue> = (0..ncol).map(|_| OwnedValue::Null).collect();
                    let (mut rid, mut cmd, mut rank) =
                        (OwnedValue::Null, OwnedValue::Null, OwnedValue::Null);
                    for (tg, v) in targets.iter().zip(r.iter()) {
                        match tg {
                            Target::Col(c) => {
                                if let Some(s) = cols.as_mut_slice().get_mut(*c) {
                                    *s = v.clone();
                                }
                            }
                            Target::Rowid => rid = v.clone(),
                            Target::Cmd => cmd = v.clone(),
                            Target::Rank => rank = v.clone(),
                        }
                    }
                    work.push((None, Some((rid, cols, cmd, rank))));
                }
            }
            FtKind::Change { select, sets } => {
                let rows = self.exec_subplan(&select, &empty).await?;
                // (UPDATE ... FROM: a row the join repeats is updated
                // once per match, the last winning, as on the pin.)
                for r in rows.iter() {
                    let old = r.as_slice().get(ncol).map(crate::func::int64_value);
                    let new = match &sets {
                        None => None,
                        Some(sets) => {
                            let mut cols = AVec::new();
                            for v in r.iter().take(ncol) {
                                cols.push(v.clone());
                            }
                            let mut rid = OwnedValue::Integer(old.unwrap_or(0));
                            for (k, tg) in sets.iter().enumerate() {
                                let Some(v) = r.as_slice().get(ncol + 1 + k) else {
                                    continue;
                                };
                                match tg {
                                    Target::Col(c) => {
                                        if let Some(s) = cols.as_mut_slice().get_mut(*c) {
                                            *s = v.clone();
                                        }
                                    }
                                    Target::Rowid => rid = v.clone(),
                                    _ => {}
                                }
                            }
                            Some((rid, cols, OwnedValue::Null, OwnedValue::Null))
                        }
                    };
                    work.push((old, new));
                }
                if sets.is_none() {
                    work.as_mut_slice().sort_by_key(|w| w.0);
                    let mut kept: AVec<Work> = AVec::new();
                    for w in work.iter() {
                        if kept.as_slice().last().is_none_or(|k| k.0 != w.0) {
                            kept.push(w.clone());
                        }
                    }
                    work = kept;
                }
            }
        }
        let t = self.conn.ft_tab(name.as_slice(), &cfg)?;
        let rt = self.conn.ft_runtime(&t).await?;
        let explicit = self.conn.explicit_txn;
        // The pin's savepoint depth: the user savepoints, and the
        // statement's own when it writes several rows.
        let user_sp = self.stmt_sp.unwrap_or_else(|| self.conn.pager.sp_depth());
        #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
        let in_prog = self.conn.in_program();
        #[cfg(not(any(feature = "triggers", feature = "foreign_keys")))]
        let in_prog = false;
        let stmt_level = explicit && multi && !in_prog;
        let init_sp = user_sp + usize::from(stmt_level);
        let mut st = self.conn.ft_take(name.as_slice(), cfg.detail, init_sp);
        let snapshot = if explicit { Some(st.clone()) } else { None };
        let r = self
            .ft_write_rows(
                &t,
                &rt,
                &mut st,
                work,
                conflict,
                stmt_level,
                user_sp,
                ret.as_slice(),
                #[cfg(feature = "fts5_delete")]
                upd,
            )
            .await;
        match r {
            Ok(()) => {
                if !explicit {
                    // The commit's xSync loads the config afresh.
                    #[cfg(feature = "fts5_delete")]
                    self.conn.fts_ver.clear();
                    let rt = self.conn.ft_runtime(&t).await?;
                    self.conn.ft_sync(&t, &rt, &mut st).await?;
                    self.conn.ft_leave(&st);
                } else {
                    self.conn.fts.push(st);
                }
                if !self.conn.explicit_txn {
                    self.conn.pager.drive_commit().await?;
                }
                self.conn.env.finish(true);
                Ok(())
            }
            Err(e) => {
                // OR FAIL keeps the rows written before the conflict
                // and so their pending terms (the pin's halt releases
                // the statement savepoint; v0.17 review fix: they were
                // dropped): the commit or the transaction flushes them.
                let kept = conflict == ConflictAction::Fail
                    && matches!(
                        e,
                        Error::Schema {
                            kind: SchemaKind::UniqueViolation,
                        }
                    );
                if kept {
                    self.conn.fts.push(st);
                } else if let Some(s) = snapshot {
                    self.conn.fts.push(s);
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn ft_write_rows(
        &mut self,
        t: &Tab,
        rt: &Runtime,
        st: &mut FtState,
        work: AVec<Work>,
        conflict: ConflictAction,
        stmt_level: bool,
        user_sp: usize,
        ret: &[RExpr],
        #[cfg(feature = "fts5_delete")] upd: (usize, usize),
    ) -> Result<(), S::Error> {
        let mut rt = rt.clone();
        if stmt_level {
            // xSavepoint(user_sp) ran before the statement's savepoint
            // opened (`Statement::step`).
            st.sp = user_sp + 1;
        }
        let cfg = t.cfg.clone();
        let normal = cfg.has_content();
        let contentless = cfg.content == Content::None;
        let econflict = if normal || cfg.contentless_delete {
            conflict
        } else {
            ConflictAction::Abort
        };
        for (old, new) in work.into_iter() {
            if old.is_none()
                && !ret.is_empty()
                && let Some((rid, vals, _, _)) = &new
            {
                let mut rd = RowData::empty();
                rd.vals = vals.clone();
                rd.vals.push(OwnedValue::Null);
                rd.vals.push(OwnedValue::Null);
                let r = match Self::ft_insert_rowid(rid)? {
                    OwnedValue::Integer(r) => r,
                    _ => -1,
                };
                rd.rowids.push(Some(r));
                let v = self.eval_returning(ret, &rd).await?;
                self.ret_rows.push(v);
                self.ret_rowids.push(Some(r));
            }
            match (old, new) {
                (None, Some((rid, vals, cmd, rank))) if !matches!(cmd, OwnedValue::Null) => {
                    let rid = Self::ft_insert_rowid(&rid)?;
                    let z = crate::fts5::aux::text_of(&cmd).unwrap_or_default();
                    if !normal && eq_ci(z.as_slice(), b"delete") {
                        if cfg.contentless_delete {
                            return Err(ft_err(SchemaKind::Fts5Command));
                        }
                        if let OwnedValue::Integer(r) = rid {
                            self.ft_delete(t, &rt, st, r, Some(vals.as_slice())).await?;
                        }
                    } else {
                        self.ft_special(t, &mut rt, st, z.as_slice(), &rank).await?;
                        self.conn.env.last_rowid.set(0);
                    }
                    self.conn.env.note_change();
                }
                (Some(o), None) => {
                    if contentless && !cfg.contentless_delete {
                        return Err(ft_err(SchemaKind::Fts5Contentless));
                    }
                    self.ft_delete(t, &rt, st, o, None).await?;
                    self.conn.env.note_change();
                }
                (None, Some((rid, vals, _, _))) => {
                    let rid = Self::ft_insert_rowid(&rid)?;
                    if econflict == ConflictAction::Replace
                        && let OwnedValue::Integer(r) = rid
                    {
                        self.ft_delete(t, &rt, st, r, None).await?;
                    }
                    let r = match self.ft_content_insert(t, &rid, vals.as_slice()).await {
                        Ok(r) => r,
                        Err(Error::Schema {
                            kind: SchemaKind::UniqueViolation,
                        }) if econflict == ConflictAction::Ignore => continue,
                        Err(e) => return Err(e),
                    };
                    self.ft_index_insert(t, &rt, st, r, vals.as_slice()).await?;
                    self.conn.note_insert(0, r);
                }
                (Some(o), Some((rid, vals, _, _))) => {
                    #[cfg(not(feature = "fts5_delete"))]
                    if contentless {
                        return Err(ft_err(SchemaKind::Fts5Contentless));
                    }
                    #[cfg(feature = "fts5_delete")]
                    if contentless {
                        // fts5ContentlessUpdate: an UPDATE of UNINDEXED
                        // columns alone is a no-op; otherwise every
                        // indexed column must be set, on a
                        // contentless_delete table only.
                        // The pin compares the rowids as
                        // `sqlite3_value_int64` reads them (review fix:
                        // `'1'` counted as a move); a new rowid that is
                        // not an integer is checked after these rules
                        // (the no-op's mismatch, or the one below).
                        let (seen, unset) = upd;
                        let moved = crate::query::int_value(&rid) != o;
                        if seen == 0 && !moved {
                            // (NULL reads as rowid 0 but is no integer:
                            // the no-op's "datatype mismatch".)
                            if !matches!(Self::ft_rowid_value(&rid)?, OwnedValue::Integer(_)) {
                                return Err(ft_err(SchemaKind::DatatypeMismatch));
                            }
                            self.conn.env.note_change();
                            continue;
                        }
                        if !cfg.contentless_delete {
                            return Err(ft_err(SchemaKind::Fts5Contentless));
                        }
                        if unset > 0 {
                            return Err(ft_err(SchemaKind::Fts5ContentlessSubset));
                        }
                    }
                    // An UPDATE's new rowid must be an integer (v0.17:
                    // NULL kept the old one; the pin's "datatype
                    // mismatch").
                    let n = match Self::ft_rowid_value(&rid)? {
                        OwnedValue::Integer(n) => n,
                        _ => return Err(ft_err(SchemaKind::DatatypeMismatch)),
                    };
                    if n != o {
                        if econflict == ConflictAction::Replace {
                            self.ft_delete(t, &rt, st, o, None).await?;
                            self.ft_delete(t, &rt, st, n, None).await?;
                            let r = self
                                .ft_content_insert(t, &OwnedValue::Integer(n), vals.as_slice())
                                .await?;
                            self.ft_index_insert(t, &rt, st, r, vals.as_slice()).await?;
                        } else {
                            let r = match self
                                .ft_content_insert(t, &OwnedValue::Integer(n), vals.as_slice())
                                .await
                            {
                                Ok(r) => r,
                                Err(Error::Schema {
                                    kind: SchemaKind::UniqueViolation,
                                }) if econflict == ConflictAction::Ignore => continue,
                                Err(e) => return Err(e),
                            };
                            self.ft_delete_moved(t, &rt, st, o).await?;
                            self.ft_index_insert(t, &rt, st, r, vals.as_slice()).await?;
                        }
                    } else {
                        self.ft_delete(t, &rt, st, o, None).await?;
                        let r = self
                            .ft_content_insert(t, &OwnedValue::Integer(n), vals.as_slice())
                            .await?;
                        self.ft_index_insert(t, &rt, st, r, vals.as_slice()).await?;
                    }
                    self.conn.env.last_rowid.set(n);
                    self.conn.env.note_change();
                }
                (None, None) => {}
            }
        }
        Ok(())
    }

    /// The old row of an UPDATE that moved it (its content was read
    /// before the new row went in, `sqlite3Fts5StorageFindDeleteRow`).
    async fn ft_delete_moved(
        &mut self,
        t: &Tab,
        rt: &Runtime,
        st: &mut FtState,
        o: i64,
    ) -> Result<(), S::Error> {
        self.ft_delete(t, rt, st, o, None).await
    }

    /// `fts5SpecialInsert`.
    async fn ft_special(
        &mut self,
        t: &Tab,
        rt: &mut Runtime,
        st: &mut FtState,
        cmd: &[u8],
        val: &OwnedValue,
    ) -> Result<(), S::Error> {
        let detail = t.cfg.detail;
        let cookie = |s: &Store| s.structure().map(|x| x.cookie).unwrap_or(0);
        if eq_ci(cmd, b"delete-all") {
            if t.cfg.has_content() {
                return Err(ft_err(SchemaKind::Fts5Command));
            }
            self.ft_delete_all(t, st).await?;
        } else if eq_ci(cmd, b"rebuild") {
            if t.cfg.content == Content::None {
                return Err(ft_err(SchemaKind::Fts5Command));
            }
            self.ft_delete_all(t, st).await?;
            self.conn.ft_totals(t, st).await?;
            let rows: AVec<(i64, AVec<OwnedValue>)> = match t.cfg.content {
                Content::External => {
                    let q = Self::ext_sql(t, None);
                    let rows = self.ft_sql(q.as_slice()).await?;
                    rows.iter()
                        .map(|r| {
                            let id = r.as_slice().first().map_or(0, crate::func::int64_value);
                            (id, r.iter().skip(1).cloned().collect())
                        })
                        .collect()
                }
                _ => {
                    let mut out = AVec::new();
                    let root = t.content.ok_or(corrupt())?;
                    let mut cur = ScanCursor::new(root);
                    let mut buf = AVec::new();
                    let ncol = t.cfg.cols.len();
                    while let Some(id) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
                        let vals =
                            crate::record::decode_record::<S::Error>(buf.as_slice(), ncol + 1)?;
                        let mut row = AVec::new();
                        for i in 0..ncol {
                            row.push(
                                vals.as_slice()
                                    .get(i + 1)
                                    .map_or(OwnedValue::Null, |v| OwnedValue::from_value(*v)),
                            );
                        }
                        out.push((id, row));
                    }
                    out
                }
            };
            for (id, vals) in rows.iter() {
                self.ft_begin(t, rt, st, false, *id).await?;
                let sizes = Self::ft_tokens(&t.cfg, st, *id, vals.as_slice(), false);
                if let Some(tot) = st.totals.as_mut() {
                    for (a, s) in tot.1.as_mut_slice().iter_mut().zip(sizes.iter()) {
                        *a = a.wrapping_add(*s);
                    }
                    tot.0 += 1;
                }
                if let Some(ds) = t.docsize {
                    let mut blob = AVec::new();
                    for s in sizes.iter() {
                        crate::fts5::pv(&mut blob, *s as u64);
                    }
                    self.conn
                        .ft_put(ds, *id, &[Value::Null, Value::Blob(blob.as_slice())])
                        .await?;
                }
            }
            if let Some(tot) = st.totals.clone() {
                let b = encode_totals(&tot);
                self.conn
                    .ft_put(
                        t.data,
                        crate::fts5::AVERAGES_ROWID,
                        &[Value::Null, Value::Blob(b.as_slice())],
                    )
                    .await?;
            }
        } else if eq_ci(cmd, b"optimize") {
            self.conn.ft_flush_index(t, rt, st).await?;
            let mut store = self.conn.ft_store(t).await?;
            crate::fts5::optimize(&mut store, &rt.t, detail).map_err(|_| corrupt())?;
            self.conn.ft_apply(t, store.log.as_slice()).await?;
        } else if eq_ci(cmd, b"merge") {
            let n = i64::from(crate::func::int64_value(val) as i32);
            self.conn.ft_flush_index(t, rt, st).await?;
            let mut store = self.conn.ft_store(t).await?;
            crate::fts5::user_merge(&mut store, &rt.t, detail, n).map_err(|_| corrupt())?;
            self.conn.ft_apply(t, store.log.as_slice()).await?;
        } else if eq_ci(cmd, b"integrity-check") {
            // `rank` 1 also checks an external table's content (the
            // pin's `iArg`; v0.17: it was always checked).
            let arg = crate::func::int64_value(val) as i32 != 0;
            self.ft_integrity(t, st, arg).await?;
        } else if eq_ci(cmd, b"flush") {
            self.conn.ft_sync(t, rt, st).await?;
        } else {
            self.conn.ft_sync(t, rt, st).await?;
            let mut probe = rt.clone();
            probe
                .set(cmd, val)
                .map_err(|_| ft_err(SchemaKind::Fts5Command))?;
            // sqlite3_value_numeric_type() converted the value in
            // place: numeric text is stored as its integer (v0.16 review
            // fix: `'64'` was stored as text).
            let stored = match (eq_ci(cmd, b"rank"), crate::fts5::config::int_value(val)) {
                (false, Some(i)) => OwnedValue::Integer(i),
                _ => val.clone(),
            };
            self.conn.ft_config_put(t, cmd, &stored).await?;
            // The structure record's cookie moves on.
            let store = self.conn.ft_store(t).await?;
            let mut s = store.structure().map_err(|_| corrupt())?;
            s.cookie = cookie(&store).wrapping_add(1);
            let b = s.encode();
            self.conn
                .ft_put(
                    t.data,
                    crate::fts5::STRUCTURE_ROWID,
                    &[Value::Null, Value::Blob(b.as_slice())],
                )
                .await?;
            #[cfg(feature = "fts5_delete")]
            self.conn.ft_hold(t, None, Some(probe.secure_delete));
            *rt = probe;
        }
        Ok(())
    }

    /// `sqlite3Fts5StorageDeleteAll`.
    async fn ft_delete_all(&mut self, t: &Tab, st: &mut FtState) -> Result<(), S::Error> {
        st.totals = None;
        let cookie = {
            let store = self.conn.ft_store(t).await?;
            store.structure().map(|s| s.cookie).unwrap_or(0)
        };
        for root in [Some(t.data), t.docsize].into_iter().flatten() {
            let mut gone = AVec::new();
            let mut cur = ScanCursor::new(root);
            let mut buf = AVec::new();
            while let Some(id) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
                gone.push(id);
            }
            for id in gone.iter() {
                btree::btree_delete(&mut self.conn.pager, root, *id).await?;
            }
            if root == t.data {
                // DELETE FROM %_idx.
                let orders = binary_orders(2);
                let mut cur = btree::IndexCursor::with_fields(t.idx, orders.as_slice(), 0);
                let mut recs: AVec<AVec<u8>> = AVec::new();
                while let Some(e) = cur.next_entry(&mut self.conn.pager).await? {
                    let vals: AVec<Value<'_>> = e.values.iter().map(|v| v.as_value()).collect();
                    let mut rec = AVec::new();
                    encode_record(vals.as_slice(), &mut rec);
                    recs.push(rec);
                }
                for r in recs.iter() {
                    btree::index_delete_core(
                        &mut self.conn.pager,
                        t.idx,
                        r.as_slice(),
                        orders.as_slice(),
                    )
                    .await?;
                }
            }
        }
        st.discard();
        let mut store = Store::default();
        store.reinit(cookie, t.cfg.contentless_delete);
        self.conn.ft_apply(t, store.log.as_slice()).await?;
        self.conn
            .ft_config_put(t, b"version", &OwnedValue::Integer(4))
            .await
    }

    /// `integrity-check`: the index decodes, its doclists match the
    /// content's tokens, and the totals and docsize rows agree.
    async fn ft_integrity(&mut self, t: &Tab, st: &mut FtState, arg: bool) -> Result<(), S::Error> {
        #[allow(unused_mut)]
        let mut store = self.conn.ft_store(t).await?;
        #[cfg(feature = "fts5_delete")]
        st.overlay(&mut store);
        let detail = t.cfg.detail;
        let reader =
            crate::fts5::Reader::new(&store, Some(&st.hash), detail).map_err(|_| corrupt())?;
        if t.cfg.content == Content::None || (t.cfg.content == Content::External && !arg) {
            return Ok(());
        }
        // The expected index: every row's tokens.
        let mut tmp = FtState::new(AVec::new(), detail, 0, 0);
        let rows: AVec<(i64, AVec<OwnedValue>)> = match t.cfg.content {
            Content::External => {
                let q = Self::ext_sql(t, None);
                let rows = self.ft_sql(q.as_slice()).await?;
                rows.iter()
                    .map(|r| {
                        let id = r.as_slice().first().map_or(0, crate::func::int64_value);
                        (id, r.iter().skip(1).cloned().collect())
                    })
                    .collect()
            }
            _ => {
                let mut out = AVec::new();
                let root = t.content.ok_or(corrupt())?;
                let mut cur = ScanCursor::new(root);
                let mut buf = AVec::new();
                let ncol = t.cfg.cols.len();
                while let Some(id) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
                    let vals = crate::record::decode_record::<S::Error>(buf.as_slice(), ncol + 1)?;
                    let mut row = AVec::new();
                    for i in 0..ncol {
                        row.push(
                            vals.as_slice()
                                .get(i + 1)
                                .map_or(OwnedValue::Null, |v| OwnedValue::from_value(*v)),
                        );
                    }
                    out.push((id, row));
                }
                out
            }
        };
        let mut sorted = rows;
        sorted.as_mut_slice().sort_by_key(|r| r.0);
        let mut ntot: AVec<i64> = t.cfg.cols.iter().map(|_| 0).collect();
        for (id, vals) in sorted.iter() {
            let sizes = Self::ft_tokens(&t.cfg, &mut tmp, *id, vals.as_slice(), false);
            for (a, s) in ntot.as_mut_slice().iter_mut().zip(sizes.iter()) {
                *a += *s;
            }
            if let Some(ds) = t.docsize {
                let Some(p) = btree::seek_rowid(&mut self.conn.pager, ds, *id).await? else {
                    return Err(corrupt());
                };
                let mut want = AVec::new();
                for s in sizes.iter() {
                    crate::fts5::pv(&mut want, *s as u64);
                }
                match field::<S::Error>(p.as_slice(), 1)? {
                    OwnedValue::Blob(b) if b == want => {}
                    _ => return Err(corrupt()),
                }
            }
        }
        let want = crate::fts5::Reader::new(&Store::with_empty(), Some(&tmp.hash), detail)
            .map_err(|_| corrupt())?;
        let got_terms = reader.all_terms();
        let want_terms = want.all_terms();
        if got_terms.len() != want_terms.len() {
            return Err(corrupt());
        }
        for (g, w) in got_terms.iter().zip(want_terms.iter()) {
            if g.0 != w.0 || g.1.len() != w.1.len() {
                return Err(corrupt());
            }
            for (x, y) in g.1.iter().zip(w.1.iter()) {
                if x.rowid != y.rowid || x.pl != y.pl {
                    return Err(corrupt());
                }
            }
        }
        self.conn.ft_totals(t, st).await?;
        if let Some(tot) = &st.totals
            && (tot.0 != sorted.len() as i64 || tot.1 != ntot)
        {
            return Err(corrupt());
        }
        Ok(())
    }

    // -----------------------------------------------------------
    // Reads
    // -----------------------------------------------------------

    /// `fts5FilterMethod` over fts5 source `vt`: its rows (columns,
    /// hidden table column, rank) and rowids.
    pub(super) async fn fts5_rows(
        &mut self,
        vt: &VtSrc,
        argv: &[OwnedValue],
        need: Option<usize>,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.fts5_rows_impl(vt, argv, need)).await
    }

    /// The rows of an OR of full-text constraints ([`VtSrc::ors`]):
    /// each branch's filter in turn, a rowid already returned
    /// skipped; a branch's `rowid =` keeps only the row equal to it
    /// (the pin rechecks it).
    pub(super) async fn fts5_or_rows(
        &mut self,
        vt: &VtSrc,
        row: &RowData,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let mut seen: AVec<i64> = AVec::new();
        let mut out = AVec::new();
        for (num, st, args, eq) in vt.ors.iter() {
            let mut argv = AVec::new();
            for a in args.iter() {
                argv.push(self.eval_row_sub(a, row).await?);
            }
            let mut one = vt.clone();
            one.ors.clear();
            one.idx_num = *num;
            one.idx_str = st.clone();
            for (r, id) in self.fts5_rows(&one, argv.as_slice(), None).await? {
                let Some(i) = id else {
                    continue;
                };
                if eq.iter().any(|(j, op)| {
                    argv.as_slice()
                        .get(*j)
                        .is_none_or(|v| !rowid_cmp(i, v, *op))
                }) {
                    continue;
                }
                if let Err(p) = seen.as_slice().binary_search(&i) {
                    seen.insert_at(p, i);
                    out.push((r, id));
                }
            }
        }
        Ok(out)
    }

    async fn fts5_rows_impl(
        &mut self,
        vt: &VtSrc,
        argv: &[OwnedValue],
        need: Option<usize>,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let cfg = vt.tab.fts.as_deref().ok_or(corrupt())?.clone();
        let ncol = cfg.cols.len();
        let t = self.conn.ft_tab(vt.name.as_slice(), &cfg)?;
        let rt = self.conn.ft_runtime(&t).await?;
        // The constraints, in idxStr order.
        let mut expr: Option<FtExpr> = None;
        let mut rank_arg: Option<OwnedValue> = None;
        let (mut eq, mut le, mut ge) = (None, None, None);
        let s = vt.idx_str.as_slice();
        let mut i = 0;
        let mut k = 0;
        while i < s.len() {
            let c = s[i];
            i += 1;
            let v = argv.get(k).cloned().unwrap_or(OwnedValue::Null);
            k += 1;
            match c {
                b'M' => {
                    let mut col = 0usize;
                    while let Some(d) = s.get(i).filter(|d| d.is_ascii_digit()) {
                        col = col * 10 + usize::from(d - b'0');
                        i += 1;
                    }
                    let q = crate::fts5::aux::text_of(&v).unwrap_or_default();
                    if q.as_slice().first() == Some(&b'*') {
                        return Err(Error::Unsupported);
                    }
                    let e = FtExpr::parse(q.as_slice(), &cfg, (col < ncol).then_some(col))
                        .map_err(|e| ft_err(expr_kind(e)))?;
                    expr = Some(match expr.take() {
                        Some(x) => x.and(e),
                        None => e,
                    });
                }
                b'L' | b'G' => {
                    let mut col = 0usize;
                    while let Some(d) = s.get(i).filter(|d| d.is_ascii_digit()) {
                        col = col * 10 + usize::from(d - b'0');
                        i += 1;
                    }
                    if matches!(v, OwnedValue::Null) {
                        continue;
                    }
                    let q = crate::fts5::aux::text_of(&v).unwrap_or_default();
                    if let Some(e) = FtExpr::pattern(q.as_slice(), c == b'G', col, &cfg)
                        .map_err(|e| ft_err(expr_kind(e)))?
                    {
                        expr = Some(match expr.take() {
                            Some(x) => x.and(e),
                            None => e,
                        });
                    }
                }
                b'r' => rank_arg = Some(v),
                b'=' => eq = Some(v),
                b'<' => le = Some(v),
                _ => ge = Some(v),
            }
        }
        let bound = |v: &Option<OwnedValue>| match v {
            Some(OwnedValue::Integer(i)) => Some(*i),
            Some(OwnedValue::Text(b)) => match crate::record::text_to_number(b.as_slice()) {
                Some(OwnedValue::Integer(i)) => Some(i),
                _ => None,
            },
            _ => None,
        };
        let (lo, hi) = if eq.is_some() {
            (bound(&eq), bound(&eq))
        } else {
            (bound(&ge), bound(&le))
        };
        let inside = |r: i64| lo.is_none_or(|l| r >= l) && hi.is_none_or(|h| r <= h);
        let mut out = AVec::new();
        let base = self.conn.env.fts_rows.try_borrow().map_or(0, |r| r.len());
        let Some(e) = expr else {
            // A scan or rowid lookup of the content.
            let mut rows: AVec<(i64, AVec<OwnedValue>)> = AVec::new();
            match cfg.content {
                Content::Normal => {
                    let root = t.content.ok_or(corrupt())?;
                    if let (Some(l), Some(h)) = (lo, hi)
                        && l == h
                    {
                        if let Some(v) = self.ft_lookup(&t, l).await? {
                            rows.push((l, v));
                        }
                    } else {
                        let mut cur = ScanCursor::new(root);
                        let mut buf = AVec::new();
                        while let Some(id) =
                            cur.next_row_into(&mut self.conn.pager, &mut buf).await?
                        {
                            if !inside(id) {
                                continue;
                            }
                            let vals =
                                crate::record::decode_record::<S::Error>(buf.as_slice(), ncol + 1)?;
                            let mut row = AVec::new();
                            for i in 0..ncol {
                                row.push(
                                    vals.as_slice()
                                        .get(i + 1)
                                        .map_or(OwnedValue::Null, |v| OwnedValue::from_value(*v)),
                                );
                            }
                            rows.push((id, row));
                            if need.is_some_and(|n| rows.len() >= n) {
                                break;
                            }
                        }
                    }
                }
                Content::External => {
                    let q = Self::ext_sql(&t, None);
                    let mut all: AVec<(i64, AVec<OwnedValue>)> = self
                        .ft_sql(q.as_slice())
                        .await?
                        .iter()
                        .map(|r| {
                            let id = r.as_slice().first().map_or(0, crate::func::int64_value);
                            (id, r.iter().skip(1).cloned().collect())
                        })
                        .collect();
                    all.as_mut_slice().sort_by_key(|r| r.0);
                    for r in all.into_iter() {
                        if inside(r.0) {
                            rows.push(r);
                        }
                    }
                }
                Content::None => {
                    let Some(ds) = t.docsize else {
                        return Err(ft_err(SchemaKind::Fts5Contentless));
                    };
                    let mut cur = ScanCursor::new(ds);
                    let mut buf = AVec::new();
                    while let Some(id) = cur.next_row_into(&mut self.conn.pager, &mut buf).await? {
                        if inside(id) {
                            rows.push((id, (0..ncol).map(|_| OwnedValue::Null).collect()));
                        }
                    }
                }
            }
            let tab = Rc::new(AuxTab {
                cfg: cfg.clone(),
                first: AVec::new(),
                psize: AVec::new(),
                colsets: AVec::new(),
                nhit: AVec::new(),
                nrow: 0,
                ntok: 0,
                contentless: cfg.content == Content::None,
                expr: None,
                sorted: false,
            });
            for (n, (id, vals)) in rows.into_iter().enumerate() {
                let mut row = vals.clone();
                row.push(OwnedValue::Integer((base + n) as i64));
                row.push(OwnedValue::Null);
                if let Ok(mut r) = self.conn.env.fts_rows.try_borrow_mut() {
                    r.push(AuxRow {
                        tab: tab.clone(),
                        phrases: AVec::new(),
                        texts: vals,
                        sizes: None,
                        missing: false,
                        nosize: false,
                    });
                }
                out.push((row, Some(id)));
            }
            return Ok(out);
        };
        // A full-text query.
        let store = self.conn.ft_store(&t).await?;
        let pending = self
            .conn
            .ft_pending(t.name.as_slice())
            .map(|s| s.hash.clone());
        let reader = crate::fts5::Reader::new(&store, pending.as_ref(), cfg.detail)
            .map_err(|_| corrupt())?;
        let detail = cfg.detail;
        // A prefix scan of the pending terms closes their position
        // lists; the pin then clears bDelete (`fts5SegIterHashInit`),
        // so the next write of the same rowid flushes.
        let mut scanned = false;
        let mut lookup = |k: &[u8], p: bool| {
            scanned |= p;
            reader.lookup(k, p, detail)
        };
        // FTS5_BI_ORDER_RANK (the rank sorter's rows run ascending) /
        // ROWID with DESC.
        let sorted = vt.idx_num & 1 != 0;
        let desc = vt.idx_num & 6 == 6;
        let leaves =
            crate::fts5::expr::leaf_rows(&e, detail, cfg.prefix.as_slice(), &mut lookup, desc);
        let rows = crate::fts5::expr::run(&e, leaves.as_slice(), desc);
        // What every row's auxiliary data shares.
        let mut nhit = AVec::new();
        for ip in 0..e.phrases.len() {
            nhit.push(crate::fts5::expr::phrase_rows(
                &e,
                ip,
                detail,
                cfg.prefix.as_slice(),
                &mut lookup,
            ) as i64);
        }
        if scanned {
            let epoch = self.conn.pager.txn_gen;
            if let Some(st) = self
                .conn
                .fts
                .as_mut_slice()
                .iter_mut()
                .find(|s| eq_ci(s.name.as_slice(), t.name.as_slice()) && s.epoch == epoch)
            {
                st.bdelete = false;
            }
        }
        let totals = match self
            .conn
            .ft_pending(t.name.as_slice())
            .and_then(|s| s.totals.clone())
        {
            Some(x) => x,
            None => {
                let b = store.get(crate::fts5::AVERAGES_ROWID).unwrap_or(&[]);
                decode_totals(b, ncol)
            }
        };
        let tab = Rc::new(AuxTab {
            cfg: cfg.clone(),
            first: e
                .phrases
                .iter()
                .map(|p| {
                    p.terms
                        .as_slice()
                        .first()
                        .map_or((AVec::new(), false), |t| (t.tok.clone(), t.prefix))
                })
                .collect(),
            psize: e.phrases.iter().map(|p| p.terms.len()).collect(),
            colsets: e
                .phrases
                .iter()
                .map(|p| e.leaves[p.leaf].colset.clone())
                .collect(),
            nhit,
            nrow: totals.0,
            ntok: totals.1.iter().sum(),
            contentless: cfg.content == Content::None,
            expr: (detail != Detail::Full && sorted).then(|| e.clone()),
            sorted: false,
        });
        // The rows the sorter hands back read their phrases as the
        // sorted cursor does; the rank itself is the inner, live
        // cursor's.
        let stab = if sorted && detail != Detail::Full {
            let mut x = (*tab).clone();
            x.sorted = true;
            Rc::new(x)
        } else {
            tab.clone()
        };
        let weights = self.ft_rank_weights(&rt, rank_arg.as_ref()).await?;
        let mut sorted_out: AVec<(f64, AVec<OwnedValue>, i64)> = AVec::new();
        for r in rows.iter() {
            if !inside(r.rowid) {
                continue;
            }
            // The rank sorter's bm25 asks for the row count: none is
            // corruption (`sqlite3Fts5StorageRowCount`; v0.17).
            if sorted && tab.nrow <= 0 {
                return Err(corrupt());
            }
            let mut missing = false;
            let vals = match self.ft_lookup(&t, r.rowid).await? {
                Some(v) => v,
                None if cfg.content == Content::None => {
                    (0..ncol).map(|_| OwnedValue::Null).collect()
                }
                // A row the index has but the content lacks: the
                // pin's lookup fails (corrupt for a normal table; for
                // an external one when the statement reads a content
                // column or an auxiliary function its text).
                None if cfg.content == Content::Normal || vt.content_used => {
                    return Err(corrupt());
                }
                None => {
                    missing = true;
                    (0..ncol).map(|_| OwnedValue::Null).collect()
                }
            };
            let mut nosize = false;
            let sizes = match t.docsize {
                Some(ds) => match btree::seek_rowid(&mut self.conn.pager, ds, r.rowid).await? {
                    Some(p) => {
                        let b = match field::<S::Error>(p.as_slice(), 1)? {
                            OwnedValue::Blob(b) => b,
                            _ => AVec::new(),
                        };
                        let mut i = 0;
                        let mut v = AVec::new();
                        for _ in 0..ncol {
                            v.push(crate::fts5::gv(b.as_slice(), &mut i).unwrap_or(0) as i64);
                        }
                        Some(v)
                    }
                    None => {
                        nosize = true;
                        Some((0..ncol).map(|_| 0).collect())
                    }
                },
                None => None,
            };
            if sorted && nosize {
                return Err(corrupt());
            }
            let mut aux = AuxRow {
                tab: tab.clone(),
                phrases: r.phrases.clone(),
                texts: vals.clone(),
                sizes,
                missing,
                nosize,
            };
            let live = crate::fts5::aux::bm25(&aux, weights.as_slice());
            aux.tab = stab.clone();
            // The sorted cursor shows the rank it computes again.
            let rank = if sorted && detail != Detail::Full {
                crate::fts5::aux::bm25(&aux, weights.as_slice())
            } else {
                live
            };
            let h = {
                let mut fr = self
                    .conn
                    .env
                    .fts_rows
                    .try_borrow_mut()
                    .map_err(|_| corrupt())?;
                fr.push(aux);
                fr.len() - 1
            };
            let mut row = vals;
            row.push(OwnedValue::Integer(h as i64));
            row.push(crate::fts5::aux::real_or_null(rank));
            if sorted {
                sorted_out.push((live, row, r.rowid));
                continue;
            }
            out.push((row, Some(r.rowid)));
            if need.is_some_and(|n| out.len() >= n) {
                break;
            }
        }
        if sorted {
            // The rank sorter: by the live rank, ties in rowid order.
            let desc = vt.idx_num & 4 != 0;
            sorted_out.as_mut_slice().sort_by(|a, b| {
                let o = a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal);
                if desc { o.reverse() } else { o }
            });
            for (_, row, id) in sorted_out.into_iter() {
                out.push((row, Some(id)));
                if need.is_some_and(|n| out.len() >= n) {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The bm25() weights of the rank function (`rank MATCH 'bm25(w,
    /// ...)'`, the `rank` setting, or none).
    async fn ft_rank_weights(
        &mut self,
        rt: &Runtime,
        arg: Option<&OwnedValue>,
    ) -> Result<AVec<f64>, S::Error> {
        let text = match arg {
            Some(v) => crate::fts5::aux::text_of(v).unwrap_or_default(),
            None => rt.rank.clone(),
        };
        let mut w = AVec::new();
        if text.is_empty() {
            return Ok(w);
        }
        let (name, args) = crate::fts5::config::parse_rank(text.as_slice())
            .ok_or(ft_err(SchemaKind::Fts5Command))?;
        if !eq_ci(name.as_slice(), b"bm25") {
            return Err(ft_err(SchemaKind::UnknownFunction));
        }
        if args.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(w);
        }
        let mut q = copy_vec(b"SELECT ");
        q.extend_from_slice(args.as_slice());
        let rows = self.ft_sql(q.as_slice()).await?;
        if let Some(r) = rows.as_slice().first() {
            for v in r.iter() {
                w.push(crate::func::real_value(v));
            }
        }
        Ok(w)
    }

    /// fts5vocab's rows.
    pub(super) async fn vocab_rows(
        &mut self,
        vt: &VtSrc,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        // Boxed: the statement and trigger futures that await this
        // (re-entered once per trigger level) stay small.
        Box::pin(self.vocab_rows_impl(vt)).await
    }

    async fn vocab_rows_impl(
        &mut self,
        vt: &VtSrc,
    ) -> Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
        let Some(def) = vt.tab.vocab.as_deref() else {
            return Err(corrupt());
        };
        let (table, kind) = (def.0.clone(), def.1);
        let ca = &self.conn.catalog_arena;
        let found = self
            .conn
            .catalog
            .find_vtab(ca, table.as_slice())
            .and_then(|v| v.tab.as_ref().ok())
            .and_then(|t| t.fts.as_deref().cloned());
        let Some(cfg) = found else {
            return Err(ft_err(SchemaKind::UnknownTable));
        };
        let t = self.conn.ft_tab(table.as_slice(), &cfg)?;
        // An fts5vocab cursor flushes the table (sqlite3Fts5FlushToDisk).
        self.conn.fts5_sync_one(table.as_slice(), &cfg).await?;
        let store = self.conn.ft_store(&t).await?;
        let pending = self
            .conn
            .ft_pending(table.as_slice())
            .map(|s| s.hash.clone());
        let reader = crate::fts5::Reader::new(&store, pending.as_ref(), cfg.detail)
            .map_err(|_| corrupt())?;
        let detail = cfg.detail;
        let ncol = cfg.cols.len();
        let mut out = AVec::new();
        let mut rid = 0i64;
        let text = |b: &[u8]| OwnedValue::Text(copy_vec(b));
        let nz = |v: i64| {
            if v > 0 {
                OwnedValue::Integer(v)
            } else {
                OwnedValue::Null
            }
        };
        for (key, hits) in reader.all_terms().iter() {
            if key.as_slice().first() != Some(&b'0') {
                continue;
            }
            let term = &key.as_slice()[1..];
            let mut push =
                |row: AVec<OwnedValue>, out: &mut AVec<(AVec<OwnedValue>, Option<i64>)>| {
                    rid += 1;
                    out.push((row, Some(rid)));
                };
            match kind {
                0 => {
                    let doc = hits.len() as i64;
                    let mut cnt = 0i64;
                    if detail == Detail::Full {
                        for h in hits.iter() {
                            cnt += crate::fts5::positions(h.pl.as_slice()).len() as i64;
                        }
                    }
                    let mut row = AVec::new();
                    row.push(text(term));
                    row.push(nz(doc));
                    row.push(nz(cnt));
                    push(row, &mut out);
                }
                1 => {
                    let mut doc: AVec<i64> = (0..ncol.max(1)).map(|_| 0).collect();
                    let mut cnt: AVec<i64> = (0..ncol.max(1)).map(|_| 0).collect();
                    for h in hits.iter() {
                        match detail {
                            Detail::Full => {
                                let mut last = -1i64;
                                for p in crate::fts5::positions(h.pl.as_slice()).iter() {
                                    let c = p >> 32;
                                    let Some(d) = doc.as_mut_slice().get_mut(c as usize) else {
                                        return Err(corrupt());
                                    };
                                    if c != last {
                                        *d += 1;
                                        last = c;
                                    }
                                    cnt[c as usize] += 1;
                                }
                            }
                            Detail::Column => {
                                for p in crate::fts5::positions(h.pl.as_slice()).iter() {
                                    let Some(d) = doc.as_mut_slice().get_mut(*p as usize) else {
                                        return Err(corrupt());
                                    };
                                    *d += 1;
                                }
                            }
                            Detail::None => doc[0] += 1,
                        }
                    }
                    for c in 0..ncol {
                        if doc[c] == 0 {
                            continue;
                        }
                        let mut row = AVec::new();
                        row.push(text(term));
                        row.push(if detail == Detail::None {
                            OwnedValue::Null
                        } else {
                            text(cfg.cols[c].as_slice())
                        });
                        row.push(nz(doc[c]));
                        row.push(nz(cnt[c]));
                        push(row, &mut out);
                    }
                }
                _ => {
                    for h in hits.iter() {
                        let ps = if detail == Detail::None {
                            let mut v = AVec::new();
                            v.push(-1);
                            v
                        } else {
                            crate::fts5::positions(h.pl.as_slice())
                        };
                        for p in ps.iter() {
                            let (c, off) = match detail {
                                Detail::Full => (p >> 32, Some(p & 0xFFFF_FFFF)),
                                Detail::Column => (*p, None),
                                Detail::None => (-1, None),
                            };
                            let mut row = AVec::new();
                            row.push(text(term));
                            row.push(OwnedValue::Integer(h.rowid));
                            row.push(match cfg.cols.as_slice().get(c as usize) {
                                Some(n) if c >= 0 => text(n.as_slice()),
                                _ => OwnedValue::Null,
                            });
                            row.push(off.map_or(OwnedValue::Null, OwnedValue::Integer));
                            push(row, &mut out);
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The error kind of a query that does not parse.
fn expr_kind(e: crate::fts5::expr::ExprErr) -> SchemaKind {
    use crate::fts5::expr::ExprErr;
    match e {
        ExprErr::Syntax => SchemaKind::Fts5Syntax,
        ExprErr::NoColumn => SchemaKind::Fts5NoColumn,
        ExprErr::ColumnQuery | ExprErr::PhraseQuery | ExprErr::TooDeep => {
            SchemaKind::Fts5Unsupported
        }
    }
}
