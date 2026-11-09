//! WITHOUT ROWID tables (v0.16, leaf feature `without_rowid`).
//!
//! The pin stores a WITHOUT ROWID table as an index B-tree — the
//! *clustered* tree — whose records hold the PRIMARY KEY columns
//! first (in key order, a column repeated with the same collation
//! dropped), then every other stored column in table order (VIRTUAL
//! generated columns are not stored). The key columns' collations
//! and directions order the tree. A secondary index on such a table
//! holds its own key columns and then, instead of a rowid, the
//! PRIMARY KEY columns it does not already hold with the same
//! collation, each with the key's collation and direction (the
//! pin's `isDupColumn`). There is no schema row for the PRIMARY KEY
//! "index": it is the table.
//!
//! The rest of hipp thinks in rowids: a row is a rowid and a record
//! in table column order, an index entry is a key and a rowid. This
//! module translates at the B-tree boundary, so every read and write
//! path works on WITHOUT ROWID tables unchanged:
//!
//! - a row is named by a *handle* — an `i64` the connection hands
//!   out per statement, bound to the row's PRIMARY KEY (a canonical
//!   record of its key values). Scans and index lookups bind the
//!   rows they find (the same key always gets the same handle), a
//!   new row takes a fresh one (`btree::new_rowid`), and
//!   `btree_insert` / `btree_delete` / `seek_rowid` on the table's
//!   root store, remove and find the row the handle names;
//! - the table's PRIMARY KEY is listed as an ordinary automatic
//!   index (`sqlite_autoindex_t_N`) whose root is the table's: its
//!   lookups find rows in the clustered tree, its maintenance is
//!   the row write itself — so UNIQUE checks, REPLACE, upsert and
//!   foreign-key parent lookups work as on any table;
//! - a secondary index's entries are translated both ways: `key,
//!   handle` becomes `key, key columns` on the way in, and back on
//!   the way out.
//!
//! Handles never reach a user: the resolver refuses `rowid` / `oid`
//! / `_rowid_` on these tables, as the pin does, and
//! `last_insert_rowid()` ignores their inserts.

use crate::arena::AVec;
use crate::btree::{IndexCursor, IndexEntry, KeyPart};
use crate::error::{Error, Result};
use crate::pager::Pager;
use crate::record::{KeyOrder, OwnedValue, Value, decode_record, encode_record};
use crate::storage::Storage;

/// How an index cursor's entries reach its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrView {
    /// Not looked up yet (a cursor made without the pager).
    Unknown,
    /// An ordinary tree, or a raw one read by this module.
    Plain,
    /// A WITHOUT ROWID table's clustered tree read as its PRIMARY
    /// KEY index (`key ..., handle`): index into `WrState::tables`.
    Pk(usize),
    /// A secondary index of such a table: index into
    /// `WrState::indexes`.
    Sec(usize),
}

/// One WITHOUT ROWID table's clustered tree.
#[derive(Debug, Clone)]
pub struct WrTable {
    pub root: u32,
    /// The stored-record position (the row's field in hipp's
    /// record) of each clustered field, in tree order: the key
    /// fields, then the rest.
    pub disk: AVec<usize>,
    /// Key fields in `disk`.
    pub npk: usize,
    /// Fields of hipp's stored record.
    pub nstored: usize,
    /// Per stored-record field: its position in `disk`.
    pub pos: AVec<usize>,
    /// The key fields' collations and directions.
    pub orders: AVec<KeyOrder>,
    /// Per key field: its table column (for secondary indexes).
    pub pk_cols: AVec<usize>,
}

/// One secondary index of a WITHOUT ROWID table.
#[derive(Debug, Clone)]
pub struct WrIndex {
    pub root: u32,
    /// Index into `WrState::tables`.
    pub table: usize,
    /// The index's own key columns.
    pub nkey: usize,
    /// The entry's full order: key columns, then the appended
    /// PRIMARY KEY columns.
    pub orders: AVec<KeyOrder>,
    /// Per table key field: its position in the entry.
    pub pk_src: AVec<usize>,
    /// The table key field of each appended column.
    pub extras: AVec<usize>,
}

/// A handle's binding.
#[derive(Debug, Clone)]
struct Handle {
    root: u32,
    key: AVec<u8>,
    alive: bool,
}

/// The connection's WITHOUT ROWID state, kept in its pager: the
/// trees' layouts (rebuilt with the catalog) and the statement's
/// handles (cleared when a statement is prepared or reset).
#[derive(Debug, Default)]
pub struct WrState {
    pub tables: AVec<WrTable>,
    pub indexes: AVec<WrIndex>,
    handles: AVec<Handle>,
    /// Open-addressing table of handle ids (0: empty), keyed by
    /// (root, key).
    slots: AVec<usize>,
    used: usize,
    /// Trees of raw records with no translation (VACUUM's copy
    /// writes them before the copy has a catalog).
    pub raw_roots: AVec<u32>,
}

fn hash(root: u32, key: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_le_bytes().iter().chain(key.iter()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl WrState {
    /// Replace the layouts (a catalog rebuild). Handles stay: a
    /// statement may rebuild the catalog part-way.
    pub fn set_layout(&mut self, tables: AVec<WrTable>, indexes: AVec<WrIndex>) {
        self.tables = tables;
        self.indexes = indexes;
    }

    /// Forget every handle (a new statement).
    pub fn clear_handles(&mut self) {
        self.handles.clear();
        self.slots.clear();
        self.used = 0;
    }

    pub fn table_of(&self, root: u32) -> Option<usize> {
        if root == 0 {
            return None;
        }
        self.tables.iter().position(|t| t.root == root)
    }

    pub fn index_of(&self, root: u32) -> Option<usize> {
        if root == 0 {
            return None;
        }
        self.indexes.iter().position(|i| i.root == root)
    }

    /// The tree's entries are records without a rowid.
    pub fn is_raw(&self, root: u32) -> bool {
        self.table_of(root).is_some()
            || self.index_of(root).is_some()
            || (root != 0 && self.raw_roots.iter().any(|r| *r == root))
    }

    /// The full comparison order of a WITHOUT ROWID tree.
    pub fn orders_of(&self, root: u32) -> Option<AVec<KeyOrder>> {
        match self.view_of(root) {
            WrView::Pk(t) => self.tables.as_slice().get(t).map(|t| t.orders.clone()),
            WrView::Sec(i) => self.indexes.as_slice().get(i).map(|i| i.orders.clone()),
            _ => None,
        }
    }

    pub fn view_of(&self, root: u32) -> WrView {
        if let Some(t) = self.table_of(root) {
            WrView::Pk(t)
        } else if let Some(i) = self.index_of(root) {
            WrView::Sec(i)
        } else {
            WrView::Plain
        }
    }

    fn find(&self, root: u32, key: &[u8]) -> Option<usize> {
        let n = self.slots.len();
        if n == 0 {
            return None;
        }
        let mut i = (hash(root, key) as usize) & (n - 1);
        for _ in 0..n {
            let id = *self.slots.as_slice().get(i)?;
            if id == 0 {
                return None;
            }
            if let Some(h) = self.handles.as_slice().get(id - 1)
                && h.root == root
                && h.key.as_slice() == key
            {
                return Some(id);
            }
            i = (i + 1) & (n - 1);
        }
        None
    }

    /// Point the (root, key) slot at handle `id`.
    fn index_slot(&mut self, root: u32, key: &[u8], id: usize) {
        if (self.used + 1) * 2 > self.slots.len() {
            let n = (self.slots.len() * 2).max(64);
            let mut fresh = AVec::with_capacity(n);
            for _ in 0..n {
                fresh.push(0usize);
            }
            let old = core::mem::replace(&mut self.slots, fresh);
            self.used = 0;
            for o in old.iter() {
                if *o == 0 {
                    continue;
                }
                let (r, k) = match self.handles.as_slice().get(*o - 1) {
                    Some(h) => (h.root, h.key.clone()),
                    None => continue,
                };
                self.place(r, k.as_slice(), *o);
            }
        }
        self.place(root, key, id);
    }

    fn place(&mut self, root: u32, key: &[u8], id: usize) {
        let n = self.slots.len();
        let mut i = (hash(root, key) as usize) & (n - 1);
        for _ in 0..n {
            let cur = self.slots.as_slice().get(i).copied().unwrap_or(0);
            if cur == 0 {
                if let Some(s) = self.slots.as_mut_slice().get_mut(i) {
                    *s = id;
                }
                self.used += 1;
                return;
            }
            let same = self
                .handles
                .as_slice()
                .get(cur - 1)
                .is_some_and(|h| h.root == root && h.key.as_slice() == key);
            if same {
                if let Some(s) = self.slots.as_mut_slice().get_mut(i) {
                    *s = id;
                }
                return;
            }
            i = (i + 1) & (n - 1);
        }
    }

    /// The handle of the stored row with key `key` (bound now if
    /// it has none).
    fn register(&mut self, root: u32, key: &[u8]) -> i64 {
        if let Some(id) = self.find(root, key) {
            if let Some(h) = self.handles.as_mut_slice().get_mut(id - 1) {
                h.alive = true;
            }
            return id as i64;
        }
        let mut k = AVec::new();
        k.extend_from_slice(key);
        self.handles.push(Handle {
            root,
            key: k,
            alive: true,
        });
        let id = self.handles.len();
        self.index_slot(root, key, id);
        id as i64
    }

    /// A handle for a row not stored yet (`btree::new_rowid`).
    pub fn fresh(&mut self, root: u32) -> i64 {
        self.handles.push(Handle {
            root,
            key: AVec::new(),
            alive: false,
        });
        self.handles.len() as i64
    }

    /// Handle `h` of table `root`, if bound to a key (`alive`:
    /// only while its row is known to be stored).
    fn handle(&self, root: u32, h: i64, alive: bool) -> Option<&Handle> {
        let i = usize::try_from(h).ok()?.checked_sub(1)?;
        self.handles
            .as_slice()
            .get(i)
            .filter(|x| x.root == root && !x.key.is_empty() && (x.alive || !alive))
    }

    fn bind(&mut self, root: u32, h: i64, key: &[u8]) {
        let Some(i) = usize::try_from(h).ok().and_then(|i| i.checked_sub(1)) else {
            return;
        };
        if let Some(x) = self.handles.as_mut_slice().get_mut(i) {
            x.root = root;
            x.key.clear();
            x.key.extend_from_slice(key);
            x.alive = true;
        } else {
            return;
        }
        self.index_slot(root, key, i + 1);
    }

    fn kill(&mut self, h: i64) {
        if let Some(x) = usize::try_from(h)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.handles.as_mut_slice().get_mut(i))
        {
            x.alive = false;
        }
    }

    /// The key values handle `h` is bound to, if its row exists.
    fn key_values<E>(&self, root: u32, h: i64, alive: bool) -> Result<Option<AVec<OwnedValue>>, E> {
        let Some(x) = self.handle(root, h, alive) else {
            return Ok(None);
        };
        Ok(Some(owned(x.key.as_slice())?))
    }

    /// An entry of a WITHOUT ROWID tree as its caller sees it:
    /// `key ..., handle`.
    pub fn translate<E>(&mut self, view: WrView, e: IndexEntry) -> Result<IndexEntry, E> {
        let (root, nkey, key) = match view {
            WrView::Pk(t) => {
                let tb = self.tables.as_slice().get(t).ok_or(Error::CorruptRecord)?;
                let n = tb.npk;
                let key = canon(e.values.as_slice().get(..n).ok_or(Error::CorruptRecord)?);
                (tb.root, n, key)
            }
            WrView::Sec(i) => {
                let ix = self.indexes.as_slice().get(i).ok_or(Error::CorruptRecord)?;
                let tb = self
                    .tables
                    .as_slice()
                    .get(ix.table)
                    .ok_or(Error::CorruptRecord)?;
                let mut kv: AVec<OwnedValue> = AVec::new();
                for p in ix.pk_src.iter() {
                    kv.push(
                        e.values
                            .as_slice()
                            .get(*p)
                            .ok_or(Error::CorruptRecord)?
                            .clone(),
                    );
                }
                (tb.root, ix.nkey, canon(kv.as_slice()))
            }
            _ => return Ok(e),
        };
        let h = self.register(root, key.as_slice());
        let mut out = AVec::new();
        out.extend_from_slice(
            e.values
                .as_slice()
                .get(..nkey)
                .ok_or(Error::CorruptRecord)?,
        );
        out.push(OwnedValue::Integer(h));
        Ok(IndexEntry { values: out })
    }

    /// A secondary index entry `key ..., handle` as the tree
    /// stores it, with the tree's order.
    pub fn sec_payload<E>(
        &self,
        i: usize,
        payload: &[u8],
    ) -> Result<(AVec<u8>, AVec<KeyOrder>), E> {
        let ix = self.indexes.as_slice().get(i).ok_or(Error::CorruptRecord)?;
        let vals = decode_record::<E>(payload, usize::MAX)?;
        let (key, h) = match vals.as_slice().split_last() {
            Some((Value::Integer(h), key)) if key.len() == ix.nkey => (key, *h),
            _ => return Err(Error::CorruptRecord),
        };
        let mut owned_key = AVec::new();
        for v in key {
            owned_key.push(OwnedValue::from_value(*v));
        }
        let (full, orders) = self.sec_full::<E>(i, owned_key.as_slice(), h)?;
        let mut vs: AVec<Value<'_>> = AVec::new();
        for v in full.iter() {
            vs.push(v.as_value());
        }
        let mut out = AVec::new();
        encode_record(vs.as_slice(), &mut out);
        Ok((out, orders))
    }

    /// A secondary index's full entry for `key` and handle `h`.
    pub fn sec_full<E>(
        &self,
        i: usize,
        key: &[OwnedValue],
        h: i64,
    ) -> Result<(AVec<OwnedValue>, AVec<KeyOrder>), E> {
        let ix = self.indexes.as_slice().get(i).ok_or(Error::CorruptRecord)?;
        let tb = self
            .tables
            .as_slice()
            .get(ix.table)
            .ok_or(Error::CorruptRecord)?;
        let kv = self
            .key_values::<E>(tb.root, h, false)?
            .ok_or(Error::CorruptRecord)?;
        let mut full = AVec::new();
        full.extend_from_slice(key);
        for j in ix.extras.iter() {
            full.push(kv.as_slice().get(*j).ok_or(Error::CorruptRecord)?.clone());
        }
        Ok((full, ix.orders.clone()))
    }

    /// A seek probe over `key ..., rowid` as the tree's probe.
    pub fn probe_for(&self, view: WrView, probe: &[KeyPart]) -> (AVec<KeyPart>, AVec<KeyOrder>) {
        let (nkey, orders) = match view {
            WrView::Pk(t) => match self.tables.as_slice().get(t) {
                Some(tb) => (tb.npk, tb.orders.clone()),
                None => (0, AVec::new()),
            },
            WrView::Sec(i) => match self.indexes.as_slice().get(i) {
                Some(ix) => (ix.nkey, ix.orders.clone()),
                None => (0, AVec::new()),
            },
            _ => (0, AVec::new()),
        };
        let mut out = AVec::new();
        for p in probe.iter().take(nkey) {
            out.push(p.clone());
        }
        // A part past the key columns stood for the rowid: the
        // tree's own suffix starts below every value instead (and a
        // short probe finds the first entry of its prefix).
        while out.len() < orders.len() {
            out.push(KeyPart::Min);
        }
        (out, orders)
    }
}

/// Decode a canonical key record.
fn owned<E>(key: &[u8]) -> Result<AVec<OwnedValue>, E> {
    let vals = decode_record::<E>(key, usize::MAX)?;
    let mut out = AVec::new();
    for v in vals.iter() {
        out.push(OwnedValue::from_value(*v));
    }
    Ok(out)
}

/// The canonical record of key values (what a handle binds).
fn canon(vals: &[OwnedValue]) -> AVec<u8> {
    let mut vs: AVec<Value<'_>> = AVec::new();
    for v in vals {
        vs.push(v.as_value());
    }
    let mut out = AVec::new();
    encode_record(vs.as_slice(), &mut out);
    out
}

/// hipp's stored record of a clustered entry: the fields back in
/// table order, up to the first one the entry lacks (a row stored
/// before ALTER TABLE ADD COLUMN).
fn to_row(t: &WrTable, vals: &[OwnedValue], out: &mut AVec<u8>) {
    let mut vs: AVec<Value<'_>> = AVec::new();
    for p in t.pos.iter() {
        match vals.get(*p) {
            Some(v) => vs.push(v.as_value()),
            None => break,
        }
    }
    out.clear();
    encode_record(vs.as_slice(), out);
}

/// A stored row as the clustered tree's record, and its key.
fn to_disk<E>(t: &WrTable, payload: &[u8]) -> Result<(AVec<u8>, AVec<OwnedValue>), E> {
    let vals = decode_record::<E>(payload, usize::MAX)?;
    let mut vs: AVec<Value<'_>> = AVec::new();
    let mut key = AVec::new();
    for (j, e) in t.disk.iter().enumerate() {
        let v = vals.as_slice().get(*e).copied().unwrap_or(Value::Null);
        if j < t.npk {
            key.push(OwnedValue::from_value(v));
        }
        vs.push(v);
    }
    let mut out = AVec::new();
    encode_record(vs.as_slice(), &mut out);
    Ok((out, key))
}

/// The probe for exactly `key` (every key field).
fn key_probe(key: &[OwnedValue]) -> AVec<KeyPart> {
    let mut p = AVec::new();
    for v in key {
        p.push(KeyPart::Val(v.clone()));
    }
    p
}

/// A raw cursor over table `t`'s whole clustered tree.
pub fn clustered_cursor<S: Storage>(pager: &Pager<S>, t: usize) -> IndexCursor {
    match pager.wr.tables.as_slice().get(t) {
        Some(tb) => IndexCursor::with_fields(tb.root, tb.orders.as_slice(), 0),
        None => IndexCursor::with_fields(0, &[], 0),
    }
}

/// The next row of a clustered scan: hipp's record into `buf`, its
/// handle returned.
pub async fn scan_next<S: Storage>(
    pager: &mut Pager<S>,
    t: usize,
    cur: &mut IndexCursor,
    buf: &mut AVec<u8>,
) -> Result<Option<i64>, S::Error> {
    let Some(e) = cur.next_entry(pager).await? else {
        return Ok(None);
    };
    let (root, key) = {
        let tb = pager
            .wr
            .tables
            .as_slice()
            .get(t)
            .ok_or(Error::CorruptRecord)?;
        let key = canon(
            e.values
                .as_slice()
                .get(..tb.npk)
                .ok_or(Error::CorruptRecord)?,
        );
        to_row(tb, e.values.as_slice(), buf);
        (tb.root, key)
    };
    Ok(Some(pager.wr.register(root, key.as_slice())))
}

/// The stored entry of key `key` in table `t`'s tree, if any.
async fn find_entry<S: Storage>(
    pager: &mut Pager<S>,
    tb: &WrTable,
    key: &[OwnedValue],
) -> Result<Option<IndexEntry>, S::Error> {
    let probe = key_probe(key);
    let (mut cur, found) =
        IndexCursor::seek_found(pager, tb.root, tb.orders.as_slice(), probe.as_slice()).await?;
    if !found {
        return Ok(None);
    }
    cur.next_entry(pager).await
}

fn entry_bytes(e: &IndexEntry) -> AVec<u8> {
    canon(e.values.as_slice())
}

/// `seek_rowid` on a WITHOUT ROWID table.
pub async fn seek<S: Storage>(
    pager: &mut Pager<S>,
    t: usize,
    h: i64,
) -> Result<Option<AVec<u8>>, S::Error> {
    let tb = pager
        .wr
        .tables
        .as_slice()
        .get(t)
        .ok_or(Error::CorruptRecord)?
        .clone();
    // A handle names its key, as the pin's two-pass writes name a
    // row by its key: whatever row holds the key now is the handle's
    // (a row an UPDATE moved there, after the old one was replaced).
    let Some(key) = pager.wr.key_values::<S::Error>(tb.root, h, false)? else {
        return Ok(None);
    };
    let Some(e) = find_entry(pager, &tb, key.as_slice()).await? else {
        return Ok(None);
    };
    let k = canon(
        e.values
            .as_slice()
            .get(..tb.npk)
            .ok_or(Error::CorruptRecord)?,
    );
    pager.wr.bind(tb.root, h, k.as_slice());
    let mut out = AVec::new();
    to_row(&tb, e.values.as_slice(), &mut out);
    Ok(Some(out))
}

/// `btree_insert` on a WITHOUT ROWID table: the row handle `h`
/// names becomes `payload` (its old entry, and any entry already at
/// the new key, leave the tree first).
pub async fn put<S: Storage>(
    pager: &mut Pager<S>,
    t: usize,
    h: i64,
    payload: &[u8],
) -> Result<(), S::Error> {
    let tb = pager
        .wr
        .tables
        .as_slice()
        .get(t)
        .ok_or(Error::CorruptRecord)?
        .clone();
    let (disk, key) = to_disk::<S::Error>(&tb, payload)?;
    if let Some(old) = pager.wr.key_values::<S::Error>(tb.root, h, true)?
        && let Some(e) = find_entry(pager, &tb, old.as_slice()).await?
    {
        let b = entry_bytes(&e);
        crate::btree::index_delete_core(pager, tb.root, b.as_slice(), tb.orders.as_slice()).await?;
    }
    if let Some(e) = find_entry(pager, &tb, key.as_slice()).await? {
        let k = canon(
            e.values
                .as_slice()
                .get(..tb.npk)
                .ok_or(Error::CorruptRecord)?,
        );
        if let Some(other) = pager.wr.find(tb.root, k.as_slice()) {
            pager.wr.kill(other as i64);
        }
        let b = entry_bytes(&e);
        crate::btree::index_delete_core(pager, tb.root, b.as_slice(), tb.orders.as_slice()).await?;
    }
    crate::btree::index_insert_core(pager, tb.root, disk.as_slice(), tb.orders.as_slice()).await?;
    let k = canon(key.as_slice());
    pager.wr.bind(tb.root, h, k.as_slice());
    Ok(())
}

/// `btree_delete` on a WITHOUT ROWID table.
pub async fn delete<S: Storage>(pager: &mut Pager<S>, t: usize, h: i64) -> Result<bool, S::Error> {
    let tb = pager
        .wr
        .tables
        .as_slice()
        .get(t)
        .ok_or(Error::CorruptRecord)?
        .clone();
    let Some(key) = pager.wr.key_values::<S::Error>(tb.root, h, true)? else {
        return Ok(false);
    };
    let Some(e) = find_entry(pager, &tb, key.as_slice()).await? else {
        pager.wr.kill(h);
        return Ok(false);
    };
    let b = entry_bytes(&e);
    crate::btree::index_delete_core(pager, tb.root, b.as_slice(), tb.orders.as_slice()).await?;
    pager.wr.kill(h);
    Ok(true)
}

/// `index_prefix_rowids` on a WITHOUT ROWID table's trees: the
/// handles of the rows whose key starts with `prefix`.
pub async fn prefix_handles<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    view: WrView,
    prefix: &[OwnedValue],
) -> Result<AVec<i64>, S::Error> {
    let mut probe: AVec<KeyPart> = AVec::new();
    for v in prefix {
        probe.push(KeyPart::Val(v.clone()));
    }
    let (probe, orders) = pager.wr.probe_for(view, probe.as_slice());
    let mut probe = probe;
    while probe.len() < orders.len() {
        probe.push(KeyPart::Min);
    }
    let (mut cur, _) =
        IndexCursor::seek_found(pager, root, orders.as_slice(), probe.as_slice()).await?;
    let mut out = AVec::new();
    while let Some(e) = cur.next_entry(pager).await? {
        let hit = prefix.iter().enumerate().all(|(j, p)| {
            let coll = orders.as_slice().get(j).map(|o| o.coll).unwrap_or_default();
            e.values.as_slice().get(j).is_some_and(|v| {
                crate::record::compare_coll(&v.as_value(), &p.as_value(), coll)
                    == core::cmp::Ordering::Equal
            })
        });
        if !hit {
            break;
        }
        let t = pager.wr.translate::<S::Error>(view, e)?;
        out.push(t.rowid::<S::Error>()?);
    }
    Ok(out)
}

/// The trees of every WITHOUT ROWID table in `cat` (its names in
/// `arena`): what the pager's `WrState` translates by.
pub fn layouts(
    cat: &crate::schema::Catalog,
    arena: &crate::arena::Arena,
) -> (AVec<WrTable>, AVec<WrIndex>) {
    use crate::sql::handle_bytes;
    let mut tables: AVec<WrTable> = AVec::new();
    let mut indexes: AVec<WrIndex> = AVec::new();
    for t in cat.tables.iter().filter(|t| t.without_rowid) {
        let tname = handle_bytes(arena, t.name);
        let Some(pk) = cat.indexes_for(arena, tname).find(|i| i.clustered) else {
            continue;
        };
        // Stored-record position of each column (VIRTUAL generated
        // columns have none).
        let mut stored: AVec<Option<usize>> = AVec::new();
        let mut n = 0usize;
        for c in t.columns.iter() {
            if c.gen_expr.is_some() && !c.stored {
                stored.push(None);
            } else {
                stored.push(Some(n));
                n += 1;
            }
        }
        let mut disk = AVec::new();
        let mut orders = AVec::new();
        let mut pk_cols = AVec::new();
        let mut ok = true;
        for c in pk.columns.iter() {
            match c
                .col
                .and_then(|i| stored.as_slice().get(i).copied().flatten())
            {
                Some(p) => {
                    disk.push(p);
                    orders.push(KeyOrder {
                        desc: c.desc,
                        coll: c.coll,
                    });
                    pk_cols.push(c.col.unwrap_or(0));
                }
                None => ok = false,
            }
        }
        if !ok || disk.is_empty() {
            continue;
        }
        let npk = disk.len();
        for (i, p) in stored.iter().enumerate() {
            if let Some(p) = p
                && !pk_cols.iter().any(|c| *c == i)
            {
                disk.push(*p);
            }
        }
        let mut pos = AVec::new();
        for e in 0..n {
            pos.push(disk.iter().position(|d| *d == e).unwrap_or(usize::MAX));
        }
        let ti = tables.len();
        tables.push(WrTable {
            root: t.root,
            disk,
            npk,
            nstored: n,
            pos,
            orders: orders.clone(),
            pk_cols: pk_cols.clone(),
        });
        for ix in cat.indexes_for(arena, tname) {
            if ix.clustered || ix.opaque {
                continue;
            }
            let mut cols = AVec::new();
            for c in ix.columns.iter() {
                cols.push((
                    c.col,
                    KeyOrder {
                        desc: c.desc,
                        coll: c.coll,
                    },
                ));
            }
            if let Some(t) = tables.as_slice().get(ti) {
                // A UNIQUE constraint's index was made before the
                // table became WITHOUT ROWID: the pin appends its
                // key columns ascending (its `bAscKeyBug`).
                let asc = ix.origin == crate::schema::IndexOrigin::Auto;
                indexes.push(index_layout(t, ti, ix.root, cols.as_slice(), asc));
            }
        }
    }
    (tables, indexes)
}

/// The layout of a secondary index rooted at `root` on table `t`
/// (`tables[ti]`), from its key columns (a column, or `None` for an
/// expression — never the key's column, even when the expression is
/// just that column) and their orders: the table's key columns it
/// does not hold with the same collation are appended (the pin's
/// `isDupColumn`) with the key's collation and direction —
/// ascending when `asc` (a UNIQUE constraint's index).
pub fn index_layout(
    t: &WrTable,
    ti: usize,
    root: u32,
    cols: &[(Option<usize>, KeyOrder)],
    asc: bool,
) -> WrIndex {
    let nkey = cols.len();
    let mut orders = AVec::new();
    for (_, o) in cols {
        orders.push(*o);
    }
    let mut pk_src = AVec::new();
    let mut extras = AVec::new();
    for (j, pc) in t.pk_cols.iter().enumerate() {
        let coll = t
            .orders
            .as_slice()
            .get(j)
            .map(|o| o.coll)
            .unwrap_or_default();
        match cols
            .iter()
            .position(|(c, o)| *c == Some(*pc) && o.coll == coll)
        {
            Some(k) => pk_src.push(k),
            None => {
                pk_src.push(nkey + extras.len());
                extras.push(j);
                if let Some(o) = t.orders.as_slice().get(j) {
                    orders.push(KeyOrder {
                        desc: o.desc && !asc,
                        coll: o.coll,
                    });
                }
            }
        }
    }
    WrIndex {
        root,
        table: ti,
        nkey,
        orders,
        pk_src,
        extras,
    }
}

impl WrState {
    /// Lay out a new secondary index on the table rooted at
    /// `table_root` (CREATE INDEX populates it before the catalog
    /// knows it).
    pub fn add_index(&mut self, table_root: u32, root: u32, cols: &[(Option<usize>, KeyOrder)]) {
        let Some(ti) = self.table_of(table_root) else {
            return;
        };
        if self.index_of(root).is_some() {
            return;
        }
        if let Some(t) = self.tables.as_slice().get(ti) {
            let ix = index_layout(t, ti, root, cols, false);
            self.indexes.push(ix);
        }
    }
}

/// ALTER TABLE DROP COLUMN on table `t`: every clustered record
/// loses stored field `stored` (never a key field), rewritten as the
/// pin rewrites it (`record::encode_record_plain`).
pub async fn drop_field<S: Storage>(
    pager: &mut Pager<S>,
    t: usize,
    stored: usize,
) -> Result<(), S::Error> {
    let tb = pager
        .wr
        .tables
        .as_slice()
        .get(t)
        .ok_or(Error::CorruptRecord)?
        .clone();
    // The field's place in the records on disk: the key fields,
    // then the non-key stored fields before it in table order. The
    // fields before `stored` are the same columns whether `tb` is
    // the layout from before or after the schema change, so this
    // never reads the dropped field's own entry.
    let before = tb
        .pos
        .as_slice()
        .get(..stored)
        .ok_or(Error::CorruptRecord)?;
    let p = tb.npk + before.iter().filter(|&&q| q >= tb.npk).count();
    let recs = crate::btree::index_payloads(pager, tb.root).await?;
    for r in recs.iter() {
        let vals = decode_record::<S::Error>(r.as_slice(), usize::MAX)?;
        let mut keep: AVec<Value<'_>> = AVec::new();
        for (j, v) in vals.iter().enumerate() {
            if j != p {
                keep.push(*v);
            }
        }
        let mut out = AVec::new();
        crate::record::encode_record_plain(keep.as_slice(), &mut out);
        crate::btree::index_delete_core(pager, tb.root, r.as_slice(), tb.orders.as_slice()).await?;
        crate::btree::index_insert_core(pager, tb.root, out.as_slice(), tb.orders.as_slice())
            .await?;
    }
    Ok(())
}

impl WrState {
    /// Sort handles of table `root`'s rows into the table's key
    /// order (a WITHOUT ROWID table's "rowid order"); handles with
    /// no row go last. Other roots are left as they are. `enc` is
    /// the database's text encoding (its BINARY order).
    pub fn sort_handles(&self, root: u32, ids: &mut AVec<i64>, enc: crate::record::TextEnc) {
        let Some(t) = self
            .table_of(root)
            .and_then(|t| self.tables.as_slice().get(t))
        else {
            return;
        };
        let mut keyed: AVec<(Option<AVec<OwnedValue>>, i64)> = AVec::new();
        for h in ids.iter() {
            let k = self.key_values::<()>(root, *h, false).ok().flatten();
            keyed.push((k, *h));
        }
        crate::arena::sort_by(keyed.as_mut_slice(), &mut |a, b| match (&a.0, &b.0) {
            (Some(x), Some(y)) => {
                crate::btree::compare_entries(x.as_slice(), y.as_slice(), t.orders.as_slice(), enc)
            }
            (Some(_), None) => core::cmp::Ordering::Less,
            (None, Some(_)) => core::cmp::Ordering::Greater,
            (None, None) => a.1.cmp(&b.1),
        });
        ids.clear();
        for (_, h) in keyed.iter() {
            ids.push(*h);
        }
    }
}
