//! PRAGMA statements and the pragma table-valued functions
//! (v0.16, `v16`).
//!
//! Grammar: `PRAGMA [main .] name [= value | (value)]`. The pin
//! ignores a name it does not know; hipp does too. A name the pin
//! knows but hipp does not implement is refused
//! (`Error::Unsupported`), as is any write hipp cannot honour —
//! never a silent no-op where the pin would act.
//!
//! Every answer is computed from hipp's own state: the database
//! header, the catalog (re-parsing stored CREATE texts for the
//! column details the catalog does not keep), and the
//! connection's pragma flags. `integrity_check` / `quick_check`
//! walk every B-tree page with the pin's checks and message
//! shapes, then check rows against NOT NULL, STRICT, CHECK and
//! the indexes.

use super::*;
use crate::arena::ABytes;
use crate::pager::{be_u16, be_u32};
use crate::record::{Affinity, decode_varint};

/// One implemented pragma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Prag {
    ApplicationId,
    AutoVacuum,
    BusyTimeout,
    CacheSize,
    CaseSensitiveLike,
    CollationList,
    DatabaseList,
    Encoding,
    ForeignKeyList,
    ForeignKeys,
    FreelistCount,
    IgnoreCheckConstraints,
    IndexInfo,
    IndexList,
    IndexXinfo,
    IntegrityCheck,
    JournalMode,
    PageCount,
    PageSize,
    QuickCheck,
    RecursiveTriggers,
    SchemaVersion,
    Synchronous,
    TableInfo,
    TableList,
    TableXinfo,
    UserVersion,
}

const PRAGMAS: &[(&str, Prag)] = &[
    ("application_id", Prag::ApplicationId),
    ("auto_vacuum", Prag::AutoVacuum),
    ("busy_timeout", Prag::BusyTimeout),
    ("cache_size", Prag::CacheSize),
    ("case_sensitive_like", Prag::CaseSensitiveLike),
    ("collation_list", Prag::CollationList),
    ("database_list", Prag::DatabaseList),
    ("encoding", Prag::Encoding),
    ("foreign_key_list", Prag::ForeignKeyList),
    ("foreign_keys", Prag::ForeignKeys),
    ("freelist_count", Prag::FreelistCount),
    ("ignore_check_constraints", Prag::IgnoreCheckConstraints),
    ("index_info", Prag::IndexInfo),
    ("index_list", Prag::IndexList),
    ("index_xinfo", Prag::IndexXinfo),
    ("integrity_check", Prag::IntegrityCheck),
    ("journal_mode", Prag::JournalMode),
    ("page_count", Prag::PageCount),
    ("page_size", Prag::PageSize),
    ("quick_check", Prag::QuickCheck),
    ("recursive_triggers", Prag::RecursiveTriggers),
    ("schema_version", Prag::SchemaVersion),
    ("synchronous", Prag::Synchronous),
    ("table_info", Prag::TableInfo),
    ("table_list", Prag::TableList),
    ("table_xinfo", Prag::TableXinfo),
    ("user_version", Prag::UserVersion),
];

/// The pin's other pragmas (its `PRAGMA pragma_list`): known
/// there, not implemented here, so refused rather than ignored.
const REFUSED: &str = "analysis_limit automatic_index cache_spill cell_size_check \
checkpoint_fullfsync compile_options count_changes data_version default_cache_size \
defer_foreign_keys empty_result_callbacks foreign_key_check full_column_names fullfsync \
function_list hard_heap_limit incremental_vacuum journal_size_limit legacy_alter_table locking_mode \
max_page_count mmap_size module_list optimize pragma_list query_only read_uncommitted \
reverse_unordered_selects secure_delete short_column_names shrink_memory soft_heap_limit \
temp_store temp_store_directory threads trusted_schema wal_autocheckpoint wal_checkpoint \
writable_schema";

/// Look a pragma name up: `Ok(None)` for a name the pin ignores.
pub(super) fn lookup<E>(name: &[u8]) -> Result<Option<Prag>, E> {
    if let Some((_, p)) = PRAGMAS.iter().find(|(n, _)| eq_ci(n.as_bytes(), name)) {
        return Ok(Some(*p));
    }
    if REFUSED.split(' ').any(|n| eq_ci(n.as_bytes(), name)) {
        return Err(Error::Unsupported);
    }
    Ok(None)
}

impl Prag {
    /// Result columns (the pin's column names, which the
    /// table-valued forms expose).
    fn columns(self) -> &'static [&'static str] {
        match self {
            Prag::TableInfo => &["cid", "name", "type", "notnull", "dflt_value", "pk"],
            Prag::TableXinfo => &[
                "cid",
                "name",
                "type",
                "notnull",
                "dflt_value",
                "pk",
                "hidden",
            ],
            Prag::IndexList => &["seq", "name", "unique", "origin", "partial"],
            Prag::IndexInfo => &["seqno", "cid", "name"],
            Prag::IndexXinfo => &["seqno", "cid", "name", "desc", "coll", "key"],
            Prag::ForeignKeyList => &[
                "id",
                "seq",
                "table",
                "from",
                "to",
                "on_update",
                "on_delete",
                "match",
            ],
            Prag::TableList => &["schema", "name", "type", "ncol", "wr", "strict"],
            Prag::DatabaseList => &["seq", "name", "file"],
            Prag::CollationList => &["seq", "name"],
            _ => &[],
        }
    }

    /// The single result column's name of a value pragma.
    fn value_name(self) -> &'static str {
        PRAGMAS
            .iter()
            .find(|(_, p)| *p == self)
            .map_or("", |(n, _)| n)
    }

    fn width(self) -> usize {
        match self.columns().len() {
            0 => 1,
            n => n,
        }
    }

    /// Takes a table / index name argument.
    fn takes_name(self) -> bool {
        matches!(
            self,
            Prag::TableInfo
                | Prag::TableXinfo
                | Prag::IndexList
                | Prag::IndexInfo
                | Prag::IndexXinfo
                | Prag::ForeignKeyList
                | Prag::TableList
        )
    }
}

/// The connection's pragma flags (v0.16): stored and reported;
/// `ignore_check_constraints` skips CHECK on every write and in
/// `integrity_check`; `foreign_keys` makes writes to tables with
/// foreign keys refuse (hipp does not enforce them).
#[derive(Debug, Default)]
pub(super) struct PragState {
    pub foreign_keys: bool,
    pub recursive_triggers: bool,
    pub ignore_checks: bool,
    pub busy_timeout: i32,
}

/// A planned PRAGMA: `prag: None` is an ignored name.
#[derive(Debug, Clone)]
pub(super) struct PragmaPlan {
    pub prag: Option<Prag>,
    pub arg: Option<AVec<u8>>,
    pub numeric: bool,
    /// A `main.` qualifier was written (table_list then omits
    /// the temp schema's row).
    pub main_only: bool,
}

impl PragmaPlan {
    pub(super) fn width(&self) -> usize {
        self.prag.map_or(0, Prag::width)
    }
}

type Rows = AVec<AVec<OwnedValue>>;

/// A table the integrity check visits: name, root, and its
/// indexes' (name, root) in the pin's order.
type CheckedTable = (AVec<u8>, u32, AVec<(AVec<u8>, u32)>);

fn text(b: &[u8]) -> OwnedValue {
    OwnedValue::Text(copy_vec(b))
}

fn int(i: i64) -> OwnedValue {
    OwnedValue::Integer(i)
}

fn row1(v: OwnedValue) -> Rows {
    let mut r = AVec::new();
    r.push(v);
    let mut rows = AVec::new();
    rows.push(r);
    rows
}

fn push_row(rows: &mut Rows, vals: &[OwnedValue]) {
    let mut r = AVec::new();
    for v in vals {
        r.push(v.clone());
    }
    rows.push(r);
}

/// Decimal text of an integer.
fn dec(mut v: i64, out: &mut AVec<u8>) {
    if v < 0 {
        out.push(b'-');
    }
    let mut digits = [0u8; 20];
    let mut n = 0;
    loop {
        let d = (v % 10).unsigned_abs() as u8;
        if let Some(slot) = digits.get_mut(n) {
            *slot = b'0' + d;
        }
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        if let Some(d) = digits.get(i) {
            out.push(*d);
        }
    }
}

/// The pin's `sqlite3GetInt32`: an optional sign, a hex
/// (`0x...`, at most 8 digits, no sign bit) or decimal number
/// read to its first non-digit; `None` without a digit or on
/// overflow.
fn get_int32(z: &[u8]) -> Option<i32> {
    let mut i = 0usize;
    let mut neg = false;
    match z.first() {
        Some(b'-') => {
            neg = true;
            i = 1;
        }
        Some(b'+') => i = 1,
        Some(b'0')
            if matches!(z.get(1), Some(b'x' | b'X'))
                && z.get(2).is_some_and(|c| c.is_ascii_hexdigit()) =>
        {
            let mut j = 2;
            while z.get(j) == Some(&b'0') {
                j += 1;
            }
            let mut u: u32 = 0;
            let mut k = 0;
            while k < 8 {
                let Some(c) = z.get(j + k).filter(|c| c.is_ascii_hexdigit()) else {
                    break;
                };
                let d = (*c as char).to_digit(16).unwrap_or(0);
                u = (u << 4) | d;
                k += 1;
            }
            if u & 0x8000_0000 == 0 && !z.get(j + k).is_some_and(|c| c.is_ascii_hexdigit()) {
                return Some(u as i32);
            }
            return None;
        }
        _ => {}
    }
    if !z.get(i).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while z.get(i) == Some(&b'0') {
        i += 1;
    }
    let mut v: i64 = 0;
    let mut n = 0;
    while n < 11 {
        let Some(c) = z.get(i + n).filter(|c| c.is_ascii_digit()) else {
            break;
        };
        v = v * 10 + i64::from(*c - b'0');
        n += 1;
    }
    if n > 10 || v - i64::from(neg) > 2_147_483_647 {
        return None;
    }
    Some(if neg { -v } else { v } as i32)
}

/// The pin's `sqlite3Atoi`: `get_int32` or 0.
fn atoi(z: &[u8]) -> i32 {
    get_int32(z).unwrap_or(0)
}

/// The pin's `getSafetyLevel`: a number, or one of on / no / off
/// / false / yes / true / extra / full (the last two only when
/// `omit_full` is false), else `dflt`.
fn safety_level(z: &[u8], omit_full: bool, dflt: u8) -> u8 {
    if z.first().is_some_and(|c| c.is_ascii_digit()) {
        return atoi(z) as u8;
    }
    const WORDS: [(&str, u8); 8] = [
        ("on", 1),
        ("no", 0),
        ("off", 0),
        ("false", 0),
        ("yes", 1),
        ("true", 1),
        ("extra", 3),
        ("full", 2),
    ];
    for (w, v) in WORDS {
        if eq_ci(w.as_bytes(), z) && (!omit_full || v <= 1) {
            return v;
        }
    }
    dflt
}

/// The pin's `sqlite3GetBoolean`: a number is true unless 0
/// (`256` is true), else `safety_level`'s words.
fn boolean(z: &[u8]) -> bool {
    if z.first().is_some_and(|c| c.is_ascii_digit()) {
        return atoi(z) != 0;
    }
    safety_level(z, true, 0) != 0
}

/// A STRICT column's standard type name for its declaration.
fn std_type(decl: &[u8]) -> &'static [u8] {
    for t in [
        b"INT" as &[u8],
        b"INTEGER",
        b"REAL",
        b"TEXT",
        b"BLOB",
        b"ANY",
    ] {
        if eq_ci(decl.trim_ascii(), t) {
            return t;
        }
    }
    b""
}

fn coll_name(c: crate::record::Coll) -> &'static [u8] {
    match c {
        crate::record::Coll::Binary => b"BINARY",
        crate::record::Coll::NoCase => b"NOCASE",
        crate::record::Coll::Rtrim => b"RTRIM",
    }
}

fn action_name(a: u8) -> &'static [u8] {
    match a {
        1 => b"RESTRICT",
        2 => b"SET NULL",
        3 => b"SET DEFAULT",
        4 => b"CASCADE",
        _ => b"NO ACTION",
    }
}

/// A scratch arena for re-parsing stored schema text (the
/// catalog builder's bound).
fn scratch() -> Arena {
    Arena::new(Scope::Statement, 0, 1 << 20)
}

/// One object of the schema in the order the pin's schema hash
/// table walks it (see `Connection::hash_order`).
#[derive(Clone, Copy)]
enum Obj<'x> {
    Table(&'x crate::schema::TableDef),
    View(&'x crate::schema::ViewDef),
    Schema,
}

impl<S: Storage> Connection<S> {
    /// Plan a PRAGMA (validated as far as the pin's prepare
    /// validates it).
    pub(super) fn plan_pragma(
        &self,
        arena: &Arena,
        schema: Option<ABytes>,
        name: ABytes,
        value: Option<ABytes>,
        numeric: bool,
    ) -> Result<Plan, S::Error> {
        let mut temp = false;
        if let Some(h) = schema {
            let b = handle_bytes(arena, h);
            if eq_ci(b, b"temp") {
                temp = true;
            } else if !eq_ci(b, b"main") {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownDatabase,
                });
            }
        }
        let prag = lookup::<S::Error>(handle_bytes(arena, name))?;
        // hipp has no temp database: a known pragma against it
        // is refused (an unknown one is still ignored).
        if temp && prag.is_some() {
            return Err(Error::Unsupported);
        }
        let arg = value.map(|h| copy_vec(handle_bytes(arena, h)));
        if let (Some(Prag::IntegrityCheck | Prag::QuickCheck), Some(a), false) =
            (prag, &arg, numeric)
            && self
                .catalog
                .find_readable(&self.catalog_arena, a.as_slice())
                .is_none()
        {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            });
        }
        Ok(Plan::Pragma(Box::new(PragmaPlan {
            prag,
            arg,
            numeric,
            main_only: schema.is_some(),
        })))
    }

    /// The schema's tables and views in the pin's hash-table
    /// order, `sqlite_schema` included. The pin walks its schema
    /// hash table, whose order for up to three tables and views
    /// is newest first, then `sqlite_schema`; hipp uses that
    /// order for any size (its hash function is not reproduced —
    /// an honest gap for larger schemas).
    fn hash_order(&self) -> AVec<Obj<'_>> {
        let ca = &self.catalog_arena;
        let mut out = AVec::new();
        for h in self.catalog.objects.iter().rev() {
            let n = handle_bytes(ca, *h);
            if let Some(t) = self.catalog.find(ca, n) {
                out.push(Obj::Table(t));
            } else if let Some(v) = self.catalog.find_view(ca, n) {
                out.push(Obj::View(v));
            }
        }
        out.push(Obj::Schema);
        out
    }

    /// A read-only pragma's rows (also the rows of its
    /// table-valued form).
    pub(super) fn pragma_rows(&self, arena: &Arena, p: &PragmaPlan) -> Result<Rows, S::Error> {
        let Some(prag) = p.prag else {
            return Ok(AVec::new());
        };
        let arg = p.arg.as_ref().map(|a| a.as_slice());
        let h = self.pager.header();
        let ca = &self.catalog_arena;
        Ok(match prag {
            Prag::UserVersion => row1(int(i64::from(h.user_version as i32))),
            Prag::ApplicationId => row1(int(i64::from(h.application_id as i32))),
            Prag::SchemaVersion => row1(int(i64::from(h.schema_cookie as i32))),
            Prag::PageSize => row1(int(i64::from(h.page_size))),
            Prag::PageCount => row1(int(if self.pager.pristine() {
                0
            } else {
                i64::from(h.page_count)
            })),
            Prag::FreelistCount => row1(int(i64::from(h.freelist_count))),
            Prag::AutoVacuum => row1(int(i64::from(h.auto_vacuum))),
            Prag::Encoding => row1(text(b"UTF-8")),
            Prag::JournalMode => row1(text(b"delete")),
            // hipp syncs the journal before any page moves and
            // the file before the journal goes: FULL.
            Prag::Synchronous => row1(int(2)),
            Prag::CacheSize => row1(int(self.cfg.cache_pages as i64)),
            Prag::BusyTimeout => row1(int(i64::from(self.prag.busy_timeout))),
            Prag::ForeignKeys => row1(int(i64::from(self.prag.foreign_keys))),
            Prag::RecursiveTriggers => row1(int(i64::from(self.prag.recursive_triggers))),
            Prag::IgnoreCheckConstraints => row1(int(i64::from(self.prag.ignore_checks))),
            Prag::CaseSensitiveLike => AVec::new(),
            Prag::CollationList => {
                let mut rows = AVec::new();
                for (i, n) in [b"BINARY" as &[u8], b"NOCASE", b"RTRIM"].iter().enumerate() {
                    push_row(&mut rows, &[int(i as i64), text(n)]);
                }
                rows
            }
            Prag::DatabaseList => {
                let mut rows = AVec::new();
                push_row(
                    &mut rows,
                    &[
                        int(0),
                        text(b"main"),
                        text(self.pager.storage().file_name()),
                    ],
                );
                rows
            }
            Prag::TableInfo | Prag::TableXinfo => match arg {
                Some(a) => self.table_info_rows(arena, a, prag == Prag::TableXinfo)?,
                None => AVec::new(),
            },
            Prag::IndexList => match arg {
                Some(a) => self.index_list_rows(a)?,
                None => AVec::new(),
            },
            Prag::IndexInfo | Prag::IndexXinfo => match arg {
                Some(a) => self.index_info_rows(a, prag == Prag::IndexXinfo)?,
                None => AVec::new(),
            },
            Prag::ForeignKeyList => match arg {
                Some(a) => self.fk_rows(a)?,
                None => AVec::new(),
            },
            Prag::TableList => {
                let mut rows = AVec::new();
                for o in self.hash_order().iter() {
                    let (name, shown, ty, ncol, strict): (&[u8], &[u8], &[u8], usize, bool) =
                        match o {
                            Obj::Table(t) => {
                                let n = handle_bytes(ca, t.name);
                                (n, n, b"table", t.columns.len(), t.strict)
                            }
                            Obj::View(v) => {
                                let n = handle_bytes(ca, v.name);
                                (n, n, b"view", v.columns.len(), false)
                            }
                            Obj::Schema => (b"sqlite_master", b"sqlite_schema", b"table", 5, false),
                        };
                    if arg.is_some_and(|a| !eq_ci(a, name)) {
                        continue;
                    }
                    push_row(
                        &mut rows,
                        &[
                            text(b"main"),
                            text(shown),
                            text(ty),
                            int(ncol as i64),
                            int(0),
                            int(i64::from(strict)),
                        ],
                    );
                }
                if !p.main_only && arg.is_none_or(|a| eq_ci(a, b"sqlite_temp_master")) {
                    push_row(
                        &mut rows,
                        &[
                            text(b"temp"),
                            text(b"sqlite_temp_schema"),
                            text(b"table"),
                            int(5),
                            int(0),
                            int(0),
                        ],
                    );
                }
                rows
            }
            // Statement-time only (they read the file).
            Prag::IntegrityCheck | Prag::QuickCheck => return Err(Error::Unsupported),
        })
    }

    /// `table_info` / `table_xinfo` of a table, view or schema
    /// table.
    fn table_info_rows(&self, arena: &Arena, name: &[u8], x: bool) -> Result<Rows, S::Error> {
        let ca = &self.catalog_arena;
        let mut rows = AVec::new();
        let schema_cols: [(&[u8], &[u8]); 5] = [
            (b"type", b"TEXT"),
            (b"name", b"TEXT"),
            (b"tbl_name", b"TEXT"),
            (b"rootpage", b"INT"),
            (b"sql", b"TEXT"),
        ];
        if crate::schema::Catalog::is_schema_name(name)
            || eq_ci(name, b"sqlite_temp_schema")
            || eq_ci(name, b"sqlite_temp_master")
        {
            for (i, (n, d)) in schema_cols.iter().enumerate() {
                let mut r = [
                    int(i as i64),
                    text(n),
                    text(d),
                    int(0),
                    OwnedValue::Null,
                    int(0),
                    int(0),
                ];
                if !x {
                    r[6] = OwnedValue::Null;
                }
                push_row(&mut rows, &r[..if x { 7 } else { 6 }]);
            }
            return Ok(rows);
        }
        if let Some(t) = self.catalog.find(ca, name) {
            let t = t.usable::<S::Error>()?;
            let sc = scratch();
            let Ast::CreateTable {
                columns,
                pk_columns,
                ..
            } = crate::sql::parse::<S::Error>(handle_bytes(ca, t.sql), &sc)?
            else {
                return Err(Error::CorruptRecord);
            };
            let mut cid = 0i64;
            for (i, c) in columns.iter().enumerate() {
                let hidden = if c.generated {
                    if c.stored { 3 } else { 2 }
                } else {
                    0
                };
                if hidden != 0 && !x {
                    continue;
                }
                let cname = handle_bytes(&sc, c.name);
                let pk = pk_columns
                    .iter()
                    .position(|p| eq_ci(handle_bytes(&sc, *p), cname))
                    .map_or(0, |k| k as i64 + 1);
                let dflt = match c.dflt_text {
                    Some(h) => text(handle_bytes(&sc, h)),
                    None => OwnedValue::Null,
                };
                let mut r = AVec::new();
                r.push(int(if x { i as i64 } else { cid }));
                r.push(text(cname));
                r.push(text(handle_bytes(&sc, c.decl)));
                r.push(int(i64::from(c.not_null)));
                r.push(dflt);
                r.push(int(pk));
                if x {
                    r.push(int(hidden));
                }
                rows.push(r);
                cid += 1;
            }
            return Ok(rows);
        }
        if let Some(v) = self.catalog.find_view(ca, name) {
            if v.columns.is_empty() {
                // A view whose columns hipp could not derive.
                return Err(Error::Unsupported);
            }
            let types = self.view_types(arena, v);
            for (i, c) in v.columns.iter().enumerate() {
                let ty = types.as_slice().get(i).cloned().unwrap_or_default();
                let mut r = AVec::new();
                r.push(int(i as i64));
                r.push(text(handle_bytes(ca, *c)));
                r.push(OwnedValue::Text(ty));
                r.push(int(0));
                r.push(OwnedValue::Null);
                r.push(int(0));
                if x {
                    r.push(int(0));
                }
                rows.push(r);
            }
        }
        Ok(rows)
    }

    /// A view's column declared types, the pin's
    /// `sqlite3SubqueryColumnTypes`: a column reference keeps its
    /// column's declared type when the expression's affinity
    /// agrees; otherwise the affinity's standard name (`NUM` for
    /// NUMERIC), or none for an expression without affinity. The
    /// view's SELECT is planned with the views it reads inlined
    /// as derived tables; a SELECT hipp cannot plan gives no
    /// types (an honest gap).
    fn view_types(&self, arena: &Arena, v: &crate::schema::ViewDef) -> AVec<AVec<u8>> {
        let mut out = AVec::new();
        let ca = &self.catalog_arena;
        let ckpt = arena.checkpoint();
        let r = (|| -> Result<AVec<AVec<u8>>, S::Error> {
            let Ast::CreateView { mut select, .. } =
                crate::sql::parse::<S::Error>(handle_bytes(ca, v.sql), arena)?
            else {
                return Err(Error::Unsupported);
            };
            self.inline_views(arena, &mut select, 0)?;
            let plan = self.plan_subquery(&select, arena, &[])?;
            let mut types = AVec::new();
            let w = plan.width();
            for i in 0..w {
                let item = plan.items.as_slice().get(i).or_else(|| {
                    plan.values
                        .as_slice()
                        .first()
                        .and_then(|r| r.as_slice().get(i))
                });
                let mut aff = item.and_then(view_aff);
                if aff.is_some() && !plan.compound.is_empty() {
                    let mut m = 0u8;
                    for (_, tail) in plan.compound.iter() {
                        if tail.values.is_empty() {
                            m |= tail
                                .items
                                .as_slice()
                                .get(i)
                                .map_or(0, crate::rules::item_datatype);
                        }
                        for row in tail.values.iter() {
                            m |= row.as_slice().get(i).map_or(0, crate::rules::item_datatype);
                        }
                    }
                    let numeric = matches!(
                        aff,
                        Some(Affinity::Numeric | Affinity::Integer | Affinity::Real)
                    );
                    if (aff == Some(Affinity::Text) && m & 0x01 != 0) || (numeric && m & 0x02 != 0)
                    {
                        aff = Some(Affinity::Blob);
                    } else if numeric && matches!(item, Some(RExpr::Cast(..))) {
                        aff = Some(Affinity::Numeric);
                    }
                }
                let z = item.and_then(|e| self.ztype(&plan, e));
                let ty: AVec<u8> = match (z, aff) {
                    (Some(z), Some(a)) if crate::record::affinity_of(z.as_slice()) == a => z,
                    (_, Some(Affinity::Numeric)) => copy_vec(b"NUM"),
                    (_, Some(Affinity::Blob)) => copy_vec(b"BLOB"),
                    (_, Some(Affinity::Integer)) => copy_vec(b"INT"),
                    (_, Some(Affinity::Real)) => copy_vec(b"REAL"),
                    (_, Some(Affinity::Text)) => copy_vec(b"TEXT"),
                    (_, None) => AVec::new(),
                };
                types.push(ty);
            }
            Ok(types)
        })();
        if let Ok(t) = r {
            out = t;
        }
        arena.rewind(ckpt);
        out
    }

    /// The declared type a result item carries through (the
    /// pin's `columnType`): a table column's (none when it was
    /// declared without one), a derived table's item's, the
    /// rowid's `INTEGER`.
    fn ztype(&self, plan: &crate::query::SubPlan, e: &RExpr) -> Option<AVec<u8>> {
        match e {
            RExpr::Collate(x, _) => self.ztype(plan, x),
            RExpr::Rowid(_) => Some(copy_vec(b"INTEGER")),
            RExpr::Column(slot, ..) => {
                let mut base = 0usize;
                for sp in plan.sources.iter() {
                    if let Some(k) = slot.checked_sub(base).filter(|k| *k < sp.ncols) {
                        if let Some(d) = &sp.derived {
                            let it = d.items.as_slice().get(k).or_else(|| {
                                d.values
                                    .as_slice()
                                    .first()
                                    .and_then(|r| r.as_slice().get(k))
                            })?;
                            return self.ztype(d, it);
                        }
                        let t = self.catalog.tables.iter().find(|t| t.root == sp.root)?;
                        let decl =
                            handle_bytes(&self.catalog_arena, t.columns.as_slice().get(k)?.decl);
                        return if decl.is_empty() {
                            None
                        } else {
                            Some(copy_vec(decl))
                        };
                    }
                    base = base.saturating_add(sp.ncols);
                }
                None
            }
            _ => None,
        }
    }

    /// Replace each view named in a FROM clause (recursively) by
    /// its SELECT as a derived table, for type derivation only.
    fn inline_views(
        &self,
        arena: &Arena,
        stmt: &mut SelectStmt,
        depth: usize,
    ) -> Result<(), S::Error> {
        if depth > 32 {
            return Err(Error::Unsupported);
        }
        let ca = &self.catalog_arena;
        for s in stmt.from.as_mut_slice().iter_mut() {
            if let Some(d) = s.derived.as_mut() {
                self.inline_views(arena, d, depth + 1)?;
                continue;
            }
            let n = handle_bytes(arena, s.table);
            if self.catalog.find(ca, n).is_some() {
                continue;
            }
            if let Some(v) = self.catalog.find_view(ca, n) {
                let Ast::CreateView { mut select, .. } =
                    crate::sql::parse::<S::Error>(handle_bytes(ca, v.sql), arena)?
                else {
                    return Err(Error::Unsupported);
                };
                self.inline_views(arena, &mut select, depth + 1)?;
                if s.alias.is_none() {
                    s.alias = Some(s.table);
                }
                s.derived = Some(Box::new(select));
            }
        }
        for (_, c) in stmt.compound.as_mut_slice().iter_mut() {
            self.inline_views(arena, c, depth + 1)?;
        }
        Ok(())
    }

    /// The indexes of table `t` in the pin's `pIndex` list order
    /// (newest first).
    fn table_indexes<'x>(&'x self, t: &'x [u8]) -> AVec<&'x crate::schema::IndexDef> {
        let ca = &self.catalog_arena;
        let mut v: AVec<&crate::schema::IndexDef> = AVec::new();
        for i in self.catalog.indexes_for(ca, t) {
            v.push(i);
        }
        let mut out = AVec::new();
        for i in v.iter().rev() {
            out.push(*i);
        }
        out
    }

    fn index_list_rows(&self, name: &[u8]) -> Result<Rows, S::Error> {
        let ca = &self.catalog_arena;
        let mut rows = AVec::new();
        let Some(t) = self.catalog.find(ca, name) else {
            return Ok(rows);
        };
        let tname = handle_bytes(ca, t.name);
        // The k-th autoindex of the table serves its k-th
        // constraint set (catalog order).
        let mut auto_pos: AVec<(u32, bool)> = AVec::new();
        let mut k = 0usize;
        for ix in self.catalog.indexes_for(ca, tname) {
            if ix.origin == crate::schema::IndexOrigin::Auto {
                let pk = t.auto_sets.as_slice().get(k).is_some_and(|s| s.primary_key);
                auto_pos.push((ix.root, pk));
                k += 1;
            }
        }
        for (seq, ix) in self.table_indexes(tname).iter().enumerate() {
            let origin: &[u8] = match ix.origin {
                crate::schema::IndexOrigin::Explicit => b"c",
                crate::schema::IndexOrigin::Auto => {
                    if auto_pos.iter().any(|(r, pk)| *r == ix.root && *pk) {
                        b"pk"
                    } else {
                        b"u"
                    }
                }
            };
            let partial = ix
                .sql
                .is_some_and(|h| sql_has_where::<S::Error>(handle_bytes(ca, h)));
            push_row(
                &mut rows,
                &[
                    int(seq as i64),
                    text(handle_bytes(ca, ix.name)),
                    int(i64::from(ix.unique)),
                    text(origin),
                    int(i64::from(partial)),
                ],
            );
        }
        Ok(rows)
    }

    fn index_info_rows(&self, name: &[u8], x: bool) -> Result<Rows, S::Error> {
        let ca = &self.catalog_arena;
        let mut rows = AVec::new();
        let Some(ix) = self.catalog.find_index(ca, name) else {
            return Ok(rows);
        };
        if ix.opaque {
            // An expression / partial index hipp cannot describe.
            return Err(Error::Unsupported);
        }
        let Some(t) = self.catalog.find(ca, handle_bytes(ca, ix.table)) else {
            return Ok(rows);
        };
        let n = ix.columns.len();
        for (i, c) in ix.columns.iter().enumerate() {
            let col = c.col.or(t.rowid_alias);
            let (cid, cname) = match col {
                Some(k) => (
                    k as i64,
                    t.columns
                        .as_slice()
                        .get(k)
                        .map_or(OwnedValue::Null, |c| text(handle_bytes(ca, c.name))),
                ),
                None => (-1, OwnedValue::Null),
            };
            let mut r = AVec::new();
            r.push(int(i as i64));
            r.push(int(cid));
            r.push(cname);
            if x {
                r.push(int(i64::from(c.desc)));
                r.push(text(coll_name(c.coll)));
                r.push(int(1));
            }
            rows.push(r);
        }
        if x {
            push_row(
                &mut rows,
                &[
                    int(n as i64),
                    int(-1),
                    OwnedValue::Null,
                    int(0),
                    text(b"BINARY"),
                    int(0),
                ],
            );
        }
        Ok(rows)
    }

    /// The parsed FOREIGN KEY clauses of one table.
    fn fk_rows(&self, name: &[u8]) -> Result<Rows, S::Error> {
        let ca = &self.catalog_arena;
        let mut rows = AVec::new();
        let Some(t) = self.catalog.find(ca, name) else {
            return Ok(rows);
        };
        let t = t.usable::<S::Error>()?;
        let sc = scratch();
        let Ast::CreateTable { fkeys, .. } =
            crate::sql::parse::<S::Error>(handle_bytes(ca, t.sql), &sc)?
        else {
            return Err(Error::CorruptRecord);
        };
        for (id, fk) in fkeys.iter().rev().enumerate() {
            for (seq, f) in fk.from.iter().enumerate() {
                let fb = handle_bytes(&sc, *f);
                let from = t
                    .columns
                    .iter()
                    .map(|c| handle_bytes(ca, c.name))
                    .find(|c| eq_ci(c, fb))
                    .unwrap_or(fb);
                let to = match fk.to.as_slice().get(seq) {
                    Some(h) => text(handle_bytes(&sc, *h)),
                    None => OwnedValue::Null,
                };
                push_row(
                    &mut rows,
                    &[
                        int(id as i64),
                        int(seq as i64),
                        text(handle_bytes(&sc, fk.table)),
                        text(from),
                        to,
                        text(action_name(fk.on_update)),
                        text(action_name(fk.on_delete)),
                        text(b"NONE"),
                    ],
                );
            }
        }
        Ok(rows)
    }

    /// Whether table `name` takes part in a foreign key, as child
    /// or parent (v0.16): with `PRAGMA foreign_keys = ON` a write
    /// to it is refused, since hipp does not enforce them.
    pub(super) fn fk_guard(&self, name: &[u8]) -> Result<(), S::Error> {
        if !self.prag.foreign_keys {
            return Ok(());
        }
        let ca = &self.catalog_arena;
        let sc = scratch();
        for t in self.catalog.tables.iter() {
            sc.reset();
            let Ok(Ast::CreateTable { fkeys, .. }) =
                crate::sql::parse::<S::Error>(handle_bytes(ca, t.sql), &sc)
            else {
                continue;
            };
            let child = eq_ci(handle_bytes(ca, t.name), name);
            if fkeys
                .iter()
                .any(|f| child || eq_ci(handle_bytes(&sc, f.table), name))
            {
                return Err(Error::Unsupported);
            }
        }
        Ok(())
    }

    /// A table-valued pragma function in FROM (`pragma_table_info
    /// ('t')`), as the SELECT of its rows: `None` when `name` is
    /// no such function. Arguments must be constant (a
    /// correlated argument is refused); a second argument names
    /// the schema (`main`; any other gives no rows).
    pub(super) fn pragma_source(
        &self,
        arena: &Arena,
        name: &[u8],
        args: Option<&AVec<Expr>>,
    ) -> Result<Option<SelectStmt>, S::Error> {
        // The empty temp schema.
        if eq_ci(name, b"sqlite_temp_schema") || eq_ci(name, b"sqlite_temp_master") {
            if args.is_some() {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            }
            let sql = b"SELECT CAST(NULL AS TEXT) AS type, CAST(NULL AS TEXT) AS name, \
CAST(NULL AS TEXT) AS tbl_name, CAST(NULL AS INT) AS rootpage, CAST(NULL AS TEXT) AS sql LIMIT 0";
            return self.select_of(arena, sql).map(Some);
        }
        let Some(rest) = name
            .get(7..)
            .filter(|_| eq_ci(name.get(..7).unwrap_or_default(), b"pragma_"))
        else {
            return Ok(None);
        };
        let prag = match PRAGMAS.iter().find(|(n, _)| eq_ci(n.as_bytes(), rest)) {
            Some((_, p)) if *p != Prag::CaseSensitiveLike => *p,
            _ => return Ok(None),
        };
        if matches!(prag, Prag::IntegrityCheck | Prag::QuickCheck) {
            // These read the file, which planning cannot.
            return Err(Error::Unsupported);
        }
        let mut vals: AVec<Option<AVec<u8>>> = AVec::new();
        for e in args.map(|a| a.as_slice()).unwrap_or_default() {
            let v = match crate::query::eval_const::<S::Error>(e, arena) {
                Ok(v) => v,
                Err(Error::Schema {
                    kind: SchemaKind::UnknownColumn | SchemaKind::AmbiguousColumn,
                }) => return Err(Error::Unsupported),
                Err(e) => return Err(e),
            };
            vals.push(match v {
                OwnedValue::Null => None,
                OwnedValue::Integer(i) => {
                    let mut t = AVec::new();
                    dec(i, &mut t);
                    Some(t)
                }
                OwnedValue::Text(t) | OwnedValue::Blob(t) => Some(t),
                OwnedValue::Real(_) => return Err(Error::Unsupported),
            });
        }
        let max = if prag.takes_name() { 2 } else { 1 };
        if vals.len() > max {
            return Err(Error::Schema {
                kind: SchemaKind::WrongArgumentCount,
            });
        }
        let (arg, schema) = if prag.takes_name() {
            (
                vals.as_slice().first().cloned().flatten(),
                vals.as_slice().get(1).cloned().flatten(),
            )
        } else {
            (None, vals.as_slice().first().cloned().flatten())
        };
        let rows = if schema
            .as_ref()
            .is_some_and(|s| !eq_ci(s.as_slice(), b"main"))
            || (prag.takes_name() && prag != Prag::TableList && arg.is_none())
        {
            AVec::new()
        } else {
            self.pragma_rows(
                arena,
                &PragmaPlan {
                    prag: Some(prag),
                    arg,
                    numeric: false,
                    main_only: schema.is_some(),
                },
            )?
        };
        // The rows as `SELECT column1 AS c1, ... FROM (VALUES ...)`.
        let cols: &[&str] = match prag.columns() {
            [] => &[],
            c => c,
        };
        let one = [prag.value_name()];
        let cols = if cols.is_empty() { &one[..] } else { cols };
        let mut sql: AVec<u8> = AVec::new();
        sql.extend_from_slice(b"SELECT ");
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                sql.extend_from_slice(b", ");
            }
            if rows.is_empty() {
                sql.extend_from_slice(b"NULL");
            } else {
                sql.extend_from_slice(b"column");
                dec(i as i64 + 1, &mut sql);
            }
            sql.extend_from_slice(b" AS \"");
            sql.extend_from_slice(c.as_bytes());
            sql.push(b'"');
        }
        if rows.is_empty() {
            sql.extend_from_slice(b" LIMIT 0");
        } else {
            sql.extend_from_slice(b" FROM (VALUES ");
            for (ri, r) in rows.iter().enumerate() {
                if ri > 0 {
                    sql.push(b',');
                }
                sql.push(b'(');
                for (i, v) in r.iter().enumerate() {
                    if i > 0 {
                        sql.push(b',');
                    }
                    sql_literal(v, &mut sql);
                }
                sql.push(b')');
            }
            sql.push(b')');
        }
        self.select_of(arena, sql.as_slice()).map(Some)
    }

    fn select_of(&self, arena: &Arena, sql: &[u8]) -> Result<SelectStmt, S::Error> {
        match crate::sql::parse::<S::Error>(sql, arena)? {
            Ast::Select(s) => Ok(s),
            _ => Err(Error::Unsupported),
        }
    }
}

/// An SQL literal for a pragma value: integers in decimal, text
/// as `CAST(x'..' AS TEXT)` (any bytes survive), NULL.
fn sql_literal(v: &OwnedValue, out: &mut AVec<u8>) {
    match v {
        OwnedValue::Integer(i) => {
            // i64::MIN has no positive literal.
            if *i == i64::MIN {
                out.extend_from_slice(b"(-9223372036854775807-1)");
            } else {
                dec(*i, out);
            }
        }
        OwnedValue::Text(t) | OwnedValue::Blob(t) => {
            let blob = matches!(v, OwnedValue::Blob(_));
            if !blob {
                out.extend_from_slice(b"CAST(");
            }
            out.extend_from_slice(b"x'");
            for b in t.iter() {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push(HEX[usize::from(b >> 4)]);
                out.push(HEX[usize::from(b & 15)]);
            }
            out.push(b'\'');
            if !blob {
                out.extend_from_slice(b" AS TEXT)");
            }
        }
        _ => out.extend_from_slice(b"NULL"),
    }
}

/// Whether stored CREATE INDEX text has a WHERE clause (a
/// partial index).
fn sql_has_where<E>(sql: &[u8]) -> bool {
    let sc = scratch();
    match crate::sql::lexer::lex::<E>(sql, &sc) {
        Ok(toks) => toks.iter().any(|t| {
            matches!(t.tok, crate::sql::lexer::Tok::Ident(h, false)
                if eq_ci(handle_bytes(&sc, h), b"WHERE"))
        }),
        Err(_) => false,
    }
}

/// The affinity a view column's expression has (the pin's
/// `sqlite3ExprAffinity`, `None` for an expression with none).
fn view_aff(e: &RExpr) -> Option<Affinity> {
    match e {
        RExpr::Column(_, a, _) | RExpr::Cast(_, a) => Some(*a),
        RExpr::Collate(x, _) => view_aff(x),
        RExpr::Rowid(_) => Some(Affinity::Integer),
        RExpr::SubScalar(p) => p.items.as_slice().first().and_then(view_aff),
        _ => None,
    }
}

// ----------------------------------------------------------------
// Statement-time execution: writes and the integrity check.
// ----------------------------------------------------------------

impl<S: Storage> Statement<'_, S> {
    /// Step a PRAGMA: compute its rows on the first step, then
    /// serve them; an error met after some integrity rows were
    /// produced is returned once they have been served (as the
    /// pin returns a corruption error after its result rows).
    pub(super) async fn step_pragma(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        if !self.query_loaded {
            let rows = match self.run_pragma().await {
                Ok(r) => r,
                Err(e) => {
                    if self.conn.pager.in_txn() && !self.conn.explicit_txn {
                        let _ = self.conn.pager.rollback().await;
                    }
                    return Err(e);
                }
            };
            for _ in 0..rows.len() {
                self.query_rowids.push(None);
            }
            self.query_rows = rows;
            self.query_loaded = true;
            self.query_pos = 0;
        }
        if self.query_pos >= self.query_rows.len()
            && let Some(e) = self.deferred.take()
        {
            self.phase = Phase::Done;
            return Err(e);
        }
        self.step_query().await
    }

    async fn run_pragma(&mut self) -> Result<Rows, S::Error> {
        let Plan::Pragma(p) = &self.plan else {
            return Err(Error::Unsupported);
        };
        let p = (**p).clone();
        let Some(prag) = p.prag else {
            return Ok(AVec::new());
        };
        if matches!(prag, Prag::IntegrityCheck | Prag::QuickCheck) {
            return self.integrity(&p, prag == Prag::QuickCheck).await;
        }
        let Some(arg) = p.arg.as_ref().map(|a| copy_vec(a.as_slice())) else {
            return self.conn.pragma_rows(&self.arena, &p);
        };
        let arg = arg.as_slice();
        let pristine = self.conn.pager.pristine()
            && self.conn.catalog.tables.is_empty()
            && self.conn.catalog.views.is_empty();
        match prag {
            Prag::UserVersion | Prag::ApplicationId => {
                let off = if prag == Prag::UserVersion { 60 } else { 68 };
                self.conn
                    .pager
                    .set_header_u32(off, atoi(arg) as u32)
                    .await?;
                if !self.conn.explicit_txn {
                    self.conn.pager.drive_commit().await?;
                }
                Ok(AVec::new())
            }
            // Read-only counters: the argument is ignored.
            Prag::PageCount | Prag::FreelistCount => self.conn.pragma_rows(&self.arena, &p),
            // The pin's shell runs in defensive mode, where this
            // write does nothing; so does hipp.
            Prag::SchemaVersion => Ok(AVec::new()),
            Prag::PageSize => {
                let n = atoi(arg);
                let ok = (512..=65536).contains(&n) && (n as u32).is_power_of_two();
                if ok && pristine && n as u32 != self.conn.pager.page_size() {
                    self.conn.pager.reformat(n as u32).await?;
                }
                // Otherwise the pin, too, only records it for a
                // VACUUM (hipp has none) or ignores a bad size.
                Ok(AVec::new())
            }
            Prag::Encoding => {
                const NAMES: [(&str, bool); 8] = [
                    ("UTF8", true),
                    ("UTF-8", true),
                    ("UTF-16le", false),
                    ("UTF-16be", false),
                    ("UTF16le", false),
                    ("UTF16be", false),
                    ("UTF-16", false),
                    ("UTF16", false),
                ];
                if !pristine {
                    // Fixed once the database has content: ignored.
                    return Ok(AVec::new());
                }
                match NAMES.iter().find(|(n, _)| eq_ci(n.as_bytes(), arg)) {
                    Some((_, true)) => Ok(AVec::new()),
                    Some((_, false)) => Err(Error::UnsupportedEncoding),
                    None => Err(Error::Schema {
                        kind: SchemaKind::UnknownEncoding,
                    }),
                }
            }
            Prag::AutoVacuum => {
                let want = if eq_ci(arg, b"none") {
                    0
                } else if eq_ci(arg, b"full") {
                    1
                } else if eq_ci(arg, b"incremental") {
                    2
                } else {
                    match atoi(arg) {
                        x @ 0..=2 => x as u8,
                        _ => 0,
                    }
                };
                let cur = self.conn.pager.header().auto_vacuum;
                if want == cur || (!pristine && (want == 0 || cur == 0)) {
                    // Unchanged, or (as on the pin) a change that
                    // would need a VACUUM: nothing happens.
                    Ok(AVec::new())
                } else {
                    // hipp keeps no pointer maps.
                    Err(Error::Unsupported)
                }
            }
            Prag::JournalMode => {
                const MODES: [&str; 6] = ["delete", "persist", "off", "truncate", "memory", "wal"];
                match MODES.iter().position(|m| eq_ci(m.as_bytes(), arg)) {
                    // An unknown mode queries the current one.
                    None | Some(0) => Ok(row1(text(b"delete"))),
                    Some(_) => Err(Error::Unsupported),
                }
            }
            Prag::Synchronous => {
                let level = (u32::from(safety_level(arg, false, 1)) + 1) & 7;
                let level = if level == 0 { 1 } else { level };
                if level == 3 {
                    Ok(AVec::new())
                } else {
                    // hipp always syncs as FULL does.
                    Err(Error::Unsupported)
                }
            }
            Prag::CacheSize => {
                if i64::from(atoi(arg)) == self.conn.cfg.cache_pages as i64 {
                    Ok(AVec::new())
                } else {
                    // The page pool is fixed by `Config`.
                    Err(Error::Unsupported)
                }
            }
            Prag::BusyTimeout => {
                self.conn.prag.busy_timeout = atoi(arg).max(0);
                self.conn.pragma_rows(&self.arena, &p)
            }
            Prag::ForeignKeys => {
                // A no-op inside a transaction, as on the pin.
                if !self.conn.explicit_txn {
                    self.conn.prag.foreign_keys = boolean(arg);
                }
                Ok(AVec::new())
            }
            Prag::RecursiveTriggers => {
                self.conn.prag.recursive_triggers = boolean(arg);
                Ok(AVec::new())
            }
            Prag::IgnoreCheckConstraints => {
                self.conn.prag.ignore_checks = boolean(arg);
                Ok(AVec::new())
            }
            Prag::CaseSensitiveLike => {
                self.conn.env.case_like.set(boolean(arg));
                Ok(AVec::new())
            }
            _ => self.conn.pragma_rows(&self.arena, &p),
        }
    }

    /// `integrity_check` / `quick_check` (see the module docs):
    /// the pin's result rows, limit and order.
    async fn integrity(&mut self, p: &PragmaPlan, quick: bool) -> Result<Rows, S::Error> {
        // The limit: a number argument (the pin reads the token
        // without its sign), else 100; a name argument checks one
        // table.
        let mut mx: i64 = 100;
        let mut only: Option<AVec<u8>> = None;
        if let Some(a) = &p.arg {
            if p.numeric {
                let s = a.as_slice();
                let s = s.strip_prefix(b"-").unwrap_or(s);
                if let Some(n) = get_int32(s) {
                    mx = if n <= 0 { 100 } else { i64::from(n) };
                }
            } else {
                only = Some(a.clone());
            }
        }
        let mut out = Out {
            rows: AVec::new(),
            budget: mx,
            stop: false,
        };
        // The tables (with their indexes) in hash order.
        let mut tables: AVec<CheckedTable> = AVec::new();
        {
            let ca = &self.conn.catalog_arena;
            for o in self.conn.hash_order().iter() {
                let (name, root): (&[u8], u32) = match o {
                    Obj::Table(t) => (handle_bytes(ca, t.name), t.root),
                    Obj::Schema => (b"sqlite_master", 1),
                    Obj::View(_) => continue,
                };
                if let Some(o) = &only
                    && !(eq_ci(o.as_slice(), name)
                        || (root == 1 && crate::schema::Catalog::is_schema_name(o.as_slice())))
                {
                    continue;
                }
                let mut ixs = AVec::new();
                for ix in self.conn.table_indexes(name).iter() {
                    ixs.push((copy_vec(handle_bytes(ca, ix.name)), ix.root));
                }
                tables.push((copy_vec(name), root, ixs));
            }
        }
        let mut roots: AVec<u32> = AVec::new();
        for (_, r, ixs) in tables.iter() {
            roots.push(*r);
            for (_, ir) in ixs.iter() {
                roots.push(*ir);
            }
        }
        // 1. The B-tree check.
        let mut ck = Ck::new(&self.conn.pager, mx)?;
        let mut counts: AVec<u64> = AVec::new();
        match bt_check(
            &mut ck,
            &mut self.conn.pager,
            roots.as_slice(),
            only.is_some(),
            &mut counts,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => self.deferred = Some(e),
        }
        if !ck.msgs.is_empty() {
            let mut m: AVec<u8> = AVec::new();
            m.extend_from_slice(b"*** in database main ***\n");
            m.extend_from_slice(ck.msgs.as_slice());
            let mut r = AVec::new();
            r.push(OwnedValue::Text(m));
            out.rows.push(r);
        }
        out.budget -= ck.nerr as i64;
        out.stop = out.budget <= 0;
        if self.deferred.is_some() || out.stop {
            return Ok(out.finish());
        }
        // 2. Index entry counts against the table's.
        let mut k = 0usize;
        for (_, _, ixs) in tables.iter() {
            let tc = counts.as_slice().get(k).copied();
            k += 1;
            for (iname, _) in ixs.iter() {
                let ic = counts.as_slice().get(k).copied();
                k += 1;
                if tc != ic && !out.stop {
                    let mut m = copy_vec(b"wrong # of entries in index ");
                    m.extend_from_slice(iname.as_slice());
                    out.emit(m);
                }
            }
        }
        // 3. Row checks.
        for (tname, root, _) in tables.iter() {
            if out.stop || *root == 1 {
                continue;
            }
            if let Err(e) = self.check_rows(tname.as_slice(), quick, &mut out).await {
                self.deferred = Some(e);
                break;
            }
        }
        Ok(out.finish())
    }

    /// Check one table's rows: NOT NULL, STRICT types, CHECK
    /// (unless ignored) and, for the full check, each row's index
    /// entries.
    async fn check_rows(
        &mut self,
        tname: &[u8],
        quick: bool,
        out: &mut Out,
    ) -> Result<(), S::Error> {
        let (root, ncols, alias, defaults, affs, nn, strict, cols, rules) = {
            let ca = &self.conn.catalog_arena;
            let Some(t) = self.conn.catalog.find(ca, tname) else {
                return Ok(());
            };
            let Ok(t) = t.usable::<S::Error>() else {
                return Ok(());
            };
            let mut defaults = AVec::new();
            let mut affs = AVec::new();
            let mut nn = AVec::new();
            let mut strict = AVec::new();
            let mut cols = AVec::new();
            for c in t.columns.iter() {
                defaults.push(read_default(ca, c));
                affs.push(c.affinity);
                nn.push(c.not_null);
                strict.push(if t.strict {
                    crate::rules::StrictType::of(handle_bytes(ca, c.decl).trim_ascii())
                        .filter(|s| *s != crate::rules::StrictType::Any)
                        .map(|_| std_type(handle_bytes(ca, c.decl)))
                } else {
                    None
                });
                cols.push(copy_vec(handle_bytes(ca, c.name)));
            }
            let rules = self.conn.table_rules(&self.arena, t)?;
            (
                t.root,
                t.columns.len(),
                t.rowid_alias,
                defaults,
                affs,
                nn,
                strict,
                cols,
                rules,
            )
        };
        #[cfg(feature = "indexes")]
        let indexes = if quick {
            AVec::new()
        } else {
            let mut v = self.conn.write_index_plans(tname)?;
            // The pin's `pIndex` order: newest first.
            let mut r = AVec::new();
            while let Some(x) = v.pop() {
                r.push(x);
            }
            r
        };
        #[cfg(feature = "indexes")]
        let inames: AVec<AVec<u8>> = {
            let ca = &self.conn.catalog_arena;
            let mut n = AVec::new();
            for ix in self.conn.table_indexes(tname).iter() {
                n.push(copy_vec(handle_bytes(ca, ix.name)));
            }
            n
        };
        #[cfg(not(feature = "indexes"))]
        let _ = quick;
        // Nothing to check row by row: the pin then reads only
        // each record's header, so hipp reads no rows (a damaged
        // overflow chain is the page check's finding alone).
        #[cfg(feature = "indexes")]
        let no_index = indexes.is_empty();
        #[cfg(not(feature = "indexes"))]
        let no_index = true;
        if no_index
            && !nn.iter().any(|b| *b)
            && strict.iter().all(|s| s.is_none())
            && rules.as_ref().is_none_or(|r| r.checks.is_empty())
        {
            return walk_pages(&mut self.conn.pager, root).await;
        }
        let virt = rules.as_ref().and_then(|r| r.virt().map(copy_bools));
        let mut cursor = ScanCursor::new(root);
        let mut buf = AVec::new();
        // The pin's messages name a row by its scan ordinal.
        let mut nrow: i64 = 0;
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            if out.stop {
                return Ok(());
            }
            nrow += 1;
            // A record whose header does not decode reads as one
            // with no fields (the pin's page check reports it).
            let mut rec = match crate::rules::decode_rec::<S::Error>(
                buf.as_slice(),
                ncols,
                defaults.as_slice(),
                virt.as_ref().map(|v| v.as_slice()),
            ) {
                Ok(r) => r,
                Err(_) => {
                    let mut r = AVec::new();
                    for d in defaults.iter() {
                        r.push(d.clone());
                    }
                    r
                }
            };
            if let Some(g) = rules.as_ref().and_then(|r| r.gcols.as_ref()) {
                g.fill::<S::Error>(rec.as_mut_slice(), rowid, true)?;
            }
            let rd = crate::rules::eval_row(alias, affs.as_slice(), rec.as_slice(), rowid);
            for (i, v) in rd.vals.iter().enumerate() {
                let msg = if matches!(v, OwnedValue::Null) && nn.as_slice().get(i) == Some(&true) {
                    Some(b"NULL" as &[u8])
                } else {
                    None
                };
                if let Some(m) = msg {
                    let mut s = copy_vec(m);
                    s.extend_from_slice(b" value in ");
                    s.extend_from_slice(tname);
                    s.push(b'.');
                    s.extend_from_slice(cols.as_slice().get(i).map_or(&[][..], |c| c.as_slice()));
                    if !out.emit(s) {
                        return Ok(());
                    }
                }
                if let Some(Some(ty)) = strict.as_slice().get(i)
                    && Some(i) != alias
                    && let Some(st) = crate::rules::StrictType::of(ty)
                    && !st.admits(v)
                {
                    let mut s = copy_vec(b"non-");
                    s.extend_from_slice(ty);
                    s.extend_from_slice(b" value in ");
                    s.extend_from_slice(tname);
                    s.push(b'.');
                    s.extend_from_slice(cols.as_slice().get(i).map_or(&[][..], |c| c.as_slice()));
                    if !out.emit(s) {
                        return Ok(());
                    }
                }
            }
            if let Some(r) = &rules {
                let ctx = crate::query::Ctx::of(&rd).with(&self.conn.env);
                let mut failed = false;
                for c in r.checks.iter() {
                    if crate::query::truthy(&crate::query::eval::<S::Error>(c, &ctx)?)
                        == Some(false)
                    {
                        failed = true;
                    }
                }
                if failed {
                    let mut s = copy_vec(b"CHECK constraint failed in ");
                    s.extend_from_slice(tname);
                    if !out.emit(s) {
                        return Ok(());
                    }
                }
            }
            #[cfg(feature = "indexes")]
            for (ix, iname) in indexes.iter().zip(inames.iter()) {
                let mut key = AVec::new();
                for c in ix.cols.iter() {
                    key.push(match c.col {
                        Some(i) if Some(i) != alias => {
                            rec.as_slice().get(i).cloned().unwrap_or(OwnedValue::Null)
                        }
                        _ => OwnedValue::Integer(rowid),
                    });
                }
                let rowids = btree::index_prefix_rowids(
                    &mut self.conn.pager,
                    ix.root,
                    ix.desc.as_slice(),
                    key.as_slice(),
                )
                .await?;
                if !rowids.iter().any(|r| *r == rowid) {
                    let mut s = copy_vec(b"row ");
                    dec(nrow, &mut s);
                    s.extend_from_slice(b" missing from index ");
                    s.extend_from_slice(iname.as_slice());
                    if !out.emit(s) {
                        return Ok(());
                    }
                } else if ix.unique
                    && key_has_no_null(key.as_slice())
                    && rowids.iter().any(|r| *r > rowid)
                {
                    let mut s = copy_vec(b"non-unique entry in index ");
                    s.extend_from_slice(iname.as_slice());
                    if !out.emit(s) {
                        return Ok(());
                    }
                }
            }
        }
        let _ = nrow;
        Ok(())
    }
}

/// Visit a table tree's pages without reading its cells' payloads
/// (the pin's header-only pass over a table with nothing to check
/// per row): a page that is not a B-tree page, or a child outside
/// the file, is the corruption error the pin's scan meets.
async fn walk_pages<S: Storage>(pager: &mut Pager<S>, root: u32) -> Result<(), S::Error> {
    let mut stack: AVec<u32> = AVec::new();
    stack.push(root);
    let mut budget = pager.page_count();
    while let Some(page) = stack.pop() {
        if budget == 0 {
            return Err(Error::CorruptPage { page });
        }
        budget -= 1;
        let d = page_copy(pager, page).await?;
        let h = btree::head::<S::Error>(d.as_slice(), page)?;
        if h.is_leaf() {
            continue;
        }
        for i in 0..usize::from(h.ncell) {
            let at = h.cell_ptrs_off() + 2 * i;
            let pc = d
                .as_slice()
                .get(at..at + 2)
                .map(|_| usize::from(be_u16(d.as_slice(), at)))
                .ok_or(Error::CorruptPage { page })?;
            let child = d
                .as_slice()
                .get(pc..pc + 4)
                .map(|_| be_u32(d.as_slice(), pc))
                .ok_or(Error::CorruptPage { page })?;
            stack.push(child);
        }
        stack.push(h.rightmost);
    }
    Ok(())
}

fn copy_bools(b: &[bool]) -> AVec<bool> {
    let mut v = AVec::new();
    v.extend_from_slice(b);
    v
}

/// The integrity check's result rows under the pin's error
/// budget: the limit counts every message (each page-check
/// message, each row message); the check stops when it is spent.
struct Out {
    rows: Rows,
    budget: i64,
    stop: bool,
}

impl Out {
    /// Emit one message row; false when the check must stop.
    fn emit(&mut self, m: AVec<u8>) -> bool {
        if self.stop {
            return false;
        }
        let mut r = AVec::new();
        r.push(OwnedValue::Text(m));
        self.rows.push(r);
        self.budget -= 1;
        self.stop = self.budget <= 0;
        !self.stop
    }

    fn finish(mut self) -> Rows {
        if self.rows.is_empty() {
            self.rows = row1(text(b"ok"));
        }
        self.rows
    }
}

// ----------------------------------------------------------------
// The B-tree page check (the pin's `sqlite3BtreeIntegrityCheck`
// shapes: "Tree R page P cell C: ...", "Page N: never used",
// "Freelist: size is X but should be Y", ...).
// ----------------------------------------------------------------

#[derive(Clone, Copy)]
enum Pfx {
    None,
    Freelist,
    Page { root: u32, page: u32 },
    Cell { root: u32, page: u32, cell: u32 },
}

struct Ck {
    /// One bit per page: referenced so far.
    seen: AVec<u8>,
    npage: u32,
    usable: usize,
    page_size: usize,
    mx: i64,
    nerr: usize,
    msgs: AVec<u8>,
}

impl Ck {
    fn new<S: Storage>(pager: &Pager<S>, mx: i64) -> Result<Ck, S::Error> {
        let npage = pager.page_count();
        let bytes = npage as usize / 8 + 1;
        let mut seen = AVec::new();
        // A header can claim far more pages than memory holds:
        // fail cleanly instead of aborting on allocation.
        seen.try_reserve(bytes)?;
        for _ in 0..bytes {
            seen.push(0);
        }
        let mut ck = Ck {
            seen,
            npage,
            usable: pager.usable() as usize,
            page_size: pager.page_size() as usize,
            mx,
            nerr: 0,
            msgs: AVec::new(),
        };
        // The lock-byte page is never part of the database.
        let pending = 1_073_741_824 / pager.page_size() + 1;
        if pending <= npage {
            ck.mark(pending);
        }
        Ok(ck)
    }

    fn is_seen(&self, page: u32) -> bool {
        self.seen
            .as_slice()
            .get(page as usize / 8)
            .is_some_and(|b| b & (1 << (page % 8)) != 0)
    }

    fn mark(&mut self, page: u32) {
        if let Some(b) = self.seen.as_mut_slice().get_mut(page as usize / 8) {
            *b |= 1 << (page % 8);
        }
    }

    fn msg(&mut self, pfx: Pfx, body: &[u8]) {
        if self.mx <= 0 {
            return;
        }
        self.mx -= 1;
        self.nerr += 1;
        if !self.msgs.is_empty() {
            self.msgs.push(b'\n');
        }
        let m = &mut self.msgs;
        match pfx {
            Pfx::None => {}
            Pfx::Freelist => m.extend_from_slice(b"Freelist: "),
            Pfx::Page { root, page } => {
                m.extend_from_slice(b"Tree ");
                dec(i64::from(root), m);
                m.extend_from_slice(b" page ");
                dec(i64::from(page), m);
                m.extend_from_slice(b": ");
            }
            Pfx::Cell { root, page, cell } => {
                m.extend_from_slice(b"Tree ");
                dec(i64::from(root), m);
                m.extend_from_slice(b" page ");
                dec(i64::from(page), m);
                m.extend_from_slice(b" cell ");
                dec(i64::from(cell), m);
                m.extend_from_slice(b": ");
            }
        }
        m.extend_from_slice(body);
    }

    fn msg_n(&mut self, pfx: Pfx, a: &[u8], n: i64, b: &[u8]) {
        let mut s = copy_vec(a);
        dec(n, &mut s);
        s.extend_from_slice(b);
        self.msg(pfx, s.as_slice());
    }

    /// The pin's `checkRef`: true when `page` may not be walked.
    fn check_ref(&mut self, page: u32, pfx: Pfx) -> bool {
        if page == 0 || page > self.npage {
            self.msg_n(pfx, b"invalid page number ", i64::from(page), b"");
            return true;
        }
        if self.is_seen(page) {
            self.msg_n(pfx, b"2nd reference to page ", i64::from(page), b"");
            return true;
        }
        self.mark(page);
        false
    }
}

async fn page_copy<S: Storage>(pager: &mut Pager<S>, page: u32) -> Result<AVec<u8>, S::Error> {
    let idx = pager.load(page).await?;
    Ok(copy_vec(pager.frame_data(idx)))
}

/// The pin's `checkList`: a freelist trunk chain or an overflow
/// chain of `expected` pages from `first`.
async fn check_list<S: Storage>(
    ck: &mut Ck,
    pager: &mut Pager<S>,
    free: bool,
    first: u32,
    expected: u32,
    pfx: Pfx,
) -> Result<(), S::Error> {
    let mut n = expected;
    let start = ck.nerr;
    let mut page = first;
    while page != 0 && ck.mx > 0 {
        if ck.check_ref(page, pfx) {
            break;
        }
        n = n.wrapping_sub(1);
        let d = page_copy(pager, page).await?;
        if free {
            let leaves = d
                .as_slice()
                .get(4..8)
                .map_or(0, |_| be_u32(d.as_slice(), 4));
            if leaves as usize > ck.usable / 4 - 2 {
                ck.msg_n(
                    pfx,
                    b"freelist leaf count too big on page ",
                    i64::from(page),
                    b"",
                );
                n = n.wrapping_sub(1);
            } else {
                for i in 0..leaves as usize {
                    let off = 8 + i * 4;
                    if off + 4 <= d.len() {
                        let leaf = be_u32(d.as_slice(), off);
                        ck.check_ref(leaf, pfx);
                    }
                }
                n = n.wrapping_sub(leaves);
            }
        }
        page = if d.len() >= 4 {
            be_u32(d.as_slice(), 0)
        } else {
            0
        };
    }
    if n != 0 && ck.nerr == start {
        let mut s: AVec<u8> = AVec::new();
        s.extend_from_slice(if free {
            b"size is "
        } else {
            b"overflow list length is "
        });
        dec(i64::from(expected.wrapping_sub(n)), &mut s);
        s.extend_from_slice(b" but should be ");
        dec(i64::from(expected), &mut s);
        ck.msg(pfx, s.as_slice());
    }
    Ok(())
}

/// One parsed cell: its on-page size, rowid (table pages),
/// payload / local sizes, left child (interior pages).
struct CellInfo {
    size: usize,
    /// Where the local payload starts, from the cell's start.
    body: usize,
    key: i64,
    payload: u64,
    local: usize,
    child: u32,
}

fn parse_cell(d: &[u8], pc: usize, kind: u8, usable: usize) -> Option<CellInfo> {
    let cell = d.get(pc..)?;
    let mut off = 0usize;
    let mut child = 0;
    if kind == btree::KIND_INTERIOR_TABLE || kind == btree::KIND_INTERIOR_INDEX {
        child = be_u32(cell.get(..4)?, 0);
        off = 4;
    }
    if kind == btree::KIND_INTERIOR_TABLE {
        let (k, n) = decode_varint::<()>(cell.get(off..)?).ok()?;
        return Some(CellInfo {
            size: off + n,
            body: off + n,
            key: k as i64,
            payload: 0,
            local: 0,
            child,
        });
    }
    let (plen, n1) = decode_varint::<()>(cell.get(off..)?).ok()?;
    off += n1;
    let mut key = 0i64;
    let local_max;
    if kind == btree::KIND_LEAF_TABLE {
        let (k, n2) = decode_varint::<()>(cell.get(off..)?).ok()?;
        key = k as i64;
        off += n2;
        local_max = usable - 35;
    } else {
        local_max = (usable - 12) * 64 / 255 - 23;
    }
    let min = (usable - 12) * 32 / 255 - 23;
    let p = usize::try_from(plen).unwrap_or(usize::MAX);
    let local = if p <= local_max {
        p
    } else {
        let surplus = min + (p - min) % (usable - 4);
        if surplus <= local_max { surplus } else { min }
    };
    let mut size = off.checked_add(local)?;
    if (local as u64) < plen {
        size += 4;
    }
    Some(CellInfo {
        size: size.max(4),
        body: off,
        key,
        payload: plen,
        local,
        child,
    })
}

type TreeFuture<'a, E> = core::pin::Pin<
    Box<dyn core::future::Future<Output = core::result::Result<(i64, i64, u64), Error<E>>> + 'a>,
>;

/// The pin's `checkTreePage`: returns (depth + 1, the page's
/// minimum rowid, the entries below it — leaf cells of a table
/// tree, every cell of an index tree).
#[allow(clippy::too_many_arguments)]
fn check_tree<'a, S: Storage>(
    ck: &'a mut Ck,
    pager: &'a mut Pager<S>,
    root: u32,
    page: u32,
    v2: u32,
    parent: Pfx,
    max_key: i64,
) -> TreeFuture<'a, S::Error> {
    Box::pin(async move {
        if page == 0 {
            return Ok((0, max_key, 0));
        }
        if ck.check_ref(page, parent) {
            return Ok((0, max_key, 0));
        }
        let d = page_copy(pager, page).await?;
        let d = d.as_slice();
        let hdr = if page == 1 { 100 } else { 0 };
        let usable = ck.usable;
        let kind = d.get(hdr).copied().unwrap_or(0);
        let ncell = d
            .get(hdr + 3..hdr + 5)
            .map_or(0, |_| usize::from(be_u16(d, hdr + 3)));
        let valid = matches!(
            kind,
            btree::KIND_INTERIOR_INDEX
                | btree::KIND_INTERIOR_TABLE
                | btree::KIND_LEAF_INDEX
                | btree::KIND_LEAF_TABLE
        );
        let ppfx = Pfx::Page { root, page };
        if !valid || ncell > (ck.page_size - 8) / 6 || d.len() < hdr + 12 {
            ck.msg(ppfx, b"btreeInitPage() returns error code 11");
            return Ok((0, max_key, 0));
        }
        let leaf = kind == btree::KIND_LEAF_TABLE || kind == btree::KIND_LEAF_INDEX;
        let int_key = kind == btree::KIND_LEAF_TABLE || kind == btree::KIND_INTERIOR_TABLE;
        let cell_start = hdr + if leaf { 8 } else { 12 };
        let cs = usize::from(be_u16(d, hdr + 5));
        let content = if cs == 0 { 65536 } else { cs };
        let frag = usize::from(d.get(hdr + 7).copied().unwrap_or(0));
        // The pin's `btreeComputeFreeSpace`.
        {
            let first = cell_start + 2 * ncell;
            let last = usable - 4;
            let mut nfree = frag + content;
            let mut pc = usize::from(be_u16(d, hdr + 1));
            let mut bad = false;
            if pc > 0 {
                if pc < content {
                    bad = true;
                } else {
                    let mut next = 0;
                    let mut size = 0;
                    loop {
                        if pc > last || pc + 4 > d.len() {
                            bad = true;
                            break;
                        }
                        next = usize::from(be_u16(d, pc));
                        size = usize::from(be_u16(d, pc + 2));
                        nfree += size;
                        if next <= pc + size + 3 {
                            break;
                        }
                        pc = next;
                    }
                    if !bad && (next > 0 || pc + size > usable) {
                        bad = true;
                    }
                }
            }
            if bad || nfree > usable || nfree < first {
                ck.msg(ppfx, b"free space corruption");
                return Ok((0, max_key, 0));
            }
        }
        let mut max_key = max_key;
        let mut key_eq = true;
        let mut depth: i64 = -1;
        let mut count: u64 = 0;
        let mut cover = true;
        let mut heap: AVec<(usize, usize)> = AVec::new();
        // Entries: every cell of a leaf, and of an index's
        // interior pages (the pin counts by page, so a damaged
        // cell still counts).
        if leaf || !int_key {
            count += ncell as u64;
        }
        if !leaf {
            let right = be_u32(d, hdr + 8);
            let (dp, mk, c) = check_tree(
                ck,
                pager,
                root,
                right,
                v2,
                Pfx::Cell {
                    root,
                    page,
                    cell: v2,
                },
                max_key,
            )
            .await?;
            depth = dp;
            max_key = mk;
            count += c;
            key_eq = false;
        }
        let mut i = ncell;
        while i > 0 && ck.mx > 0 {
            i -= 1;
            let cpfx = Pfx::Cell {
                root,
                page,
                cell: i as u32,
            };
            let at = cell_start + 2 * i;
            let pc = if at + 2 <= d.len() {
                usize::from(be_u16(d, at))
            } else {
                0
            };
            if pc < content || pc > usable - 4 {
                let mut s = copy_vec(b"Offset ");
                dec(pc as i64, &mut s);
                s.extend_from_slice(b" out of range ");
                dec(content as i64, &mut s);
                s.extend_from_slice(b"..");
                dec((usable - 4) as i64, &mut s);
                ck.msg(cpfx, s.as_slice());
                cover = false;
                continue;
            }
            let info = match parse_cell(d, pc, kind, usable) {
                Some(x) if pc + x.size <= usable => x,
                _ => {
                    ck.msg(cpfx, b"Extends off end of page");
                    cover = false;
                    continue;
                }
            };
            // A table row's record header must fit its payload.
            if kind == btree::KIND_LEAF_TABLE && info.payload > 0 {
                let rec = pc + info.body;
                let hsz = d
                    .get(rec..)
                    .and_then(|r| decode_varint::<()>(r).ok())
                    .map(|(h, _)| h);
                if hsz.is_none_or(|h| h == 0 || h > info.payload) {
                    ck.msg(cpfx, b"Bad cell header size");
                    cover = false;
                    continue;
                }
            }
            if int_key {
                if if key_eq {
                    info.key > max_key
                } else {
                    info.key >= max_key
                } {
                    let mut s = copy_vec(b"Rowid ");
                    dec(info.key, &mut s);
                    s.extend_from_slice(b" out of order");
                    ck.msg(cpfx, s.as_slice());
                }
                max_key = info.key;
                key_eq = false;
            }
            if info.payload > info.local as u64 {
                let extra = info.payload - info.local as u64;
                let npage = (extra + usable as u64 - 5) / (usable as u64 - 4);
                let ovfl = d
                    .get(pc + info.size - 4..pc + info.size)
                    .map_or(0, |_| be_u32(d, pc + info.size - 4));
                check_list(ck, pager, false, ovfl, npage as u32, cpfx).await?;
            }
            if !leaf {
                let (d2, mk, c) =
                    check_tree(ck, pager, root, info.child, i as u32, cpfx, max_key).await?;
                max_key = mk;
                count += c;
                key_eq = false;
                if d2 != depth {
                    ck.msg(cpfx, b"Child page depth differs");
                    depth = d2;
                }
            } else {
                heap.push((pc, pc + info.size - 1));
            }
        }
        if cover && ck.mx > 0 {
            if !leaf {
                heap.clear();
                for j in (0..ncell).rev() {
                    let pc = usize::from(be_u16(d, cell_start + 2 * j));
                    if let Some(x) = parse_cell(d, pc, kind, usable) {
                        heap.push((pc, pc + x.size - 1));
                    }
                }
            }
            let mut fb = usize::from(be_u16(d, hdr + 1));
            while fb > 0 && fb + 4 <= d.len() {
                let size = usize::from(be_u16(d, fb + 2));
                heap.push((fb, (fb + size).saturating_sub(1)));
                let j = usize::from(be_u16(d, fb));
                if j <= fb {
                    break;
                }
                fb = j;
            }
            heap.as_mut_slice().sort_unstable_by_key(|(a, b)| (*a, *b));
            let mut nfrag: i64 = 0;
            let mut prev_end = content as i64 - 1;
            let mut emptied = true;
            let n = heap.len();
            for (k, (a, b)) in heap.iter().enumerate() {
                if prev_end >= *a as i64 {
                    let mut s = copy_vec(b"Multiple uses for byte ");
                    dec(*a as i64, &mut s);
                    s.extend_from_slice(b" of page ");
                    dec(i64::from(page), &mut s);
                    ck.msg(Pfx::None, s.as_slice());
                    emptied = k + 1 == n;
                    prev_end = -2;
                    break;
                }
                nfrag += *a as i64 - prev_end - 1;
                prev_end = *b as i64;
            }
            if prev_end != -2 {
                nfrag += usable as i64 - prev_end - 1;
            }
            if emptied && prev_end != -2 && nfrag != frag as i64 {
                let mut s = copy_vec(b"Fragmentation of ");
                dec(nfrag, &mut s);
                s.extend_from_slice(b" bytes reported as ");
                dec(frag as i64, &mut s);
                s.extend_from_slice(b" on page ");
                dec(i64::from(page), &mut s);
                ck.msg(Pfx::None, s.as_slice());
            }
        }
        Ok((depth + 1, max_key, count))
    })
}

/// The whole B-tree check: the freelist, each root's tree (its
/// entry count into `counts`), then pages nothing referenced.
/// `partial` (one table) skips the freelist and the unused pages.
async fn bt_check<S: Storage>(
    ck: &mut Ck,
    pager: &mut Pager<S>,
    roots: &[u32],
    partial: bool,
    counts: &mut AVec<u64>,
) -> Result<(), S::Error> {
    if !partial {
        let (trunk, n) = (pager.header().freelist_trunk, pager.header().freelist_count);
        check_list(ck, pager, true, trunk, n, Pfx::Freelist).await?;
    }
    for r in roots {
        if ck.mx <= 0 {
            counts.push(0);
            continue;
        }
        let (_, _, c) = check_tree(ck, pager, *r, *r, 0, Pfx::None, i64::MAX).await?;
        counts.push(c);
    }
    if !partial {
        let autovac = pager.header().auto_vacuum != 0;
        let per = (ck.usable / 5 + 1) as u32;
        for i in 1..=ck.npage {
            if ck.mx <= 0 {
                break;
            }
            let ptrmap = autovac && i >= 2 && ((i - 2) / per) * per + 2 == i;
            if !ck.is_seen(i) && !ptrmap {
                ck.msg_n(Pfx::None, b"Page ", i64::from(i), b": never used");
            }
        }
    }
    Ok(())
}
