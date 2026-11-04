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
    /// `defer_foreign_keys` (v0.16, `foreign_keys`).
    #[cfg(feature = "foreign_keys")]
    DeferForeignKeys,
    /// `foreign_key_check [table]` (v0.16, `foreign_keys`).
    #[cfg(feature = "foreign_keys")]
    ForeignKeyCheck,
    ForeignKeyList,
    ForeignKeys,
    FreelistCount,
    IgnoreCheckConstraints,
    /// `incremental_vacuum [(N)]` (v0.16, `autovacuum`).
    #[cfg(feature = "autovacuum")]
    IncrementalVacuum,
    IndexInfo,
    IndexList,
    IndexXinfo,
    IntegrityCheck,
    JournalMode,
    /// `journal_size_limit` (v0.16, `wal`).
    #[cfg(feature = "wal")]
    JournalSizeLimit,
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
    /// `wal_autocheckpoint` (v0.16, `wal`).
    #[cfg(feature = "wal")]
    WalAutocheckpoint,
    /// `wal_checkpoint [(mode)]` (v0.16, `wal`).
    #[cfg(feature = "wal")]
    WalCheckpoint,
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
    #[cfg(feature = "autovacuum")]
    ("incremental_vacuum", Prag::IncrementalVacuum),
    ("index_info", Prag::IndexInfo),
    ("index_list", Prag::IndexList),
    ("index_xinfo", Prag::IndexXinfo),
    ("integrity_check", Prag::IntegrityCheck),
    ("journal_mode", Prag::JournalMode),
    #[cfg(feature = "wal")]
    ("journal_size_limit", Prag::JournalSizeLimit),
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
    #[cfg(feature = "wal")]
    ("wal_autocheckpoint", Prag::WalAutocheckpoint),
    #[cfg(feature = "wal")]
    ("wal_checkpoint", Prag::WalCheckpoint),
];

/// The foreign-key pragmas (v0.16, `foreign_keys`; refused without
/// the leaf).
#[cfg(feature = "foreign_keys")]
const FK_PRAGMAS: &[(&str, Prag)] = &[
    ("defer_foreign_keys", Prag::DeferForeignKeys),
    ("foreign_key_check", Prag::ForeignKeyCheck),
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
    #[cfg(feature = "foreign_keys")]
    for (n, p) in FK_PRAGMAS.iter() {
        if eq_ci(n.as_bytes(), name) {
            return Ok(Some(*p));
        }
    }
    if REFUSED.split(' ').any(|n| eq_ci(n.as_bytes(), name)) {
        return Err(Error::Unsupported);
    }
    Ok(None)
}

impl Prag {
    /// A setting of the connection, not of one database: a
    /// `temp.` (or `main.`) qualifier is ignored, as on the pin.
    pub(super) fn connection_wide(self) -> bool {
        #[cfg(feature = "foreign_keys")]
        if self == Prag::DeferForeignKeys {
            return true;
        }
        #[cfg(feature = "wal")]
        if self == Prag::WalAutocheckpoint {
            return true;
        }
        // The text encoding is the connection's (v0.16, `utf16`:
        // `PRAGMA temp.encoding = 'UTF-16be'` sets it).
        #[cfg(feature = "utf16")]
        if self == Prag::Encoding {
            return true;
        }
        matches!(
            self,
            Prag::ForeignKeys
                | Prag::RecursiveTriggers
                | Prag::IgnoreCheckConstraints
                | Prag::BusyTimeout
                | Prag::CaseSensitiveLike
                | Prag::CollationList
        )
    }

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
            #[cfg(feature = "foreign_keys")]
            Prag::ForeignKeyCheck => &["table", "rowid", "parent", "fkid"],
            Prag::DatabaseList => &["seq", "name", "file"],
            #[cfg(feature = "wal")]
            Prag::WalCheckpoint => &["busy", "log", "checkpointed"],
            Prag::CollationList => &["seq", "name"],
            _ => &[],
        }
    }

    /// The single result column's name of a value pragma.
    fn value_name(self) -> &'static str {
        #[cfg(feature = "foreign_keys")]
        if self == Prag::DeferForeignKeys {
            return "defer_foreign_keys";
        }
        PRAGMAS
            .iter()
            .find(|(_, p)| *p == self)
            .map_or("", |(n, _)| n)
    }

    fn width(self) -> usize {
        // The pin's `incremental_vacuum` rows have no column (v0.16,
        // `autovacuum`).
        #[cfg(feature = "autovacuum")]
        if self == Prag::IncrementalVacuum {
            return 0;
        }
        match self.columns().len() {
            0 => 1,
            n => n,
        }
    }

    /// No table-valued form on the pin (v0.16).
    fn no_table(self) -> bool {
        #[cfg(feature = "autovacuum")]
        if self == Prag::IncrementalVacuum {
            return true;
        }
        #[cfg(feature = "wal")]
        if matches!(self, Prag::WalCheckpoint | Prag::WalAutocheckpoint) {
            return true;
        }
        false
    }

    /// `journal_size_limit`, whose table-valued form takes a schema
    /// (v0.16, `wal`).
    fn sized_journal(self) -> bool {
        #[cfg(feature = "wal")]
        if self == Prag::JournalSizeLimit {
            return true;
        }
        false
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
    /// `case_sensitive_like` has been set (to any value) on this
    /// connection: LIKE is then non-deterministic, as on the pin
    /// (`rules::gen_uses_like`).
    pub like_redefined: bool,
    /// A `PRAGMA page_size` that could not apply to the existing
    /// file: the pin keeps it for the next VACUUM (VACUUM INTO
    /// too), which builds the file with it.
    pub vacuum_page_size: Option<u32>,
    /// A `PRAGMA auto_vacuum` change waiting for the next VACUUM,
    /// as on the pin.
    pub vacuum_autovac: Option<u8>,
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
/// A table under `integrity_check`: name, root page, and its
/// indexes (name, root page, whether partial).
type CheckedTable = (AVec<u8>, u32, AVec<(AVec<u8>, u32, bool)>);

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
pub(super) fn dec(mut v: i64, out: &mut AVec<u8>) {
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

/// `p` without its value: the query form.
fn p_query(p: &PragmaPlan) -> PragmaPlan {
    PragmaPlan {
        prag: p.prag,
        arg: None,
        numeric: p.numeric,
        main_only: p.main_only,
    }
}

/// The pin's `sqlite3DecOrHexToI64` as `journal_size_limit` reads
/// it (v0.16, `wal`): `0x` and hex digits (wrapping into 64 bits),
/// else an optionally signed decimal prefix after leading spaces,
/// clamped to 64 bits; no digits read as 0.
#[cfg(feature = "wal")]
fn atoi64(z: &[u8]) -> i64 {
    if let Some(h) = z
        .strip_prefix(b"0x")
        .or_else(|| z.strip_prefix(b"0X"))
        .filter(|h| h.first().is_some_and(|c| c.is_ascii_hexdigit()))
    {
        let mut u: u64 = 0;
        for c in h.iter().take_while(|c| c.is_ascii_hexdigit()) {
            u = (u << 4) | u64::from((*c as char).to_digit(16).unwrap_or(0));
        }
        return u as i64;
    }
    let mut i = 0;
    while z.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    let neg = z.get(i) == Some(&b'-');
    if matches!(z.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let mut v: i128 = 0;
    while let Some(c) = z.get(i).filter(|c| c.is_ascii_digit()) {
        v = (v * 10 + i128::from(*c - b'0')).min(i128::from(i64::MAX) + 1);
        i += 1;
    }
    let v = if neg { -v } else { v };
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// A STRICT column's standard type name for its declaration (the
/// pin's dequoted token, `record::decl_token`).
fn std_type(decl: &[u8]) -> &'static [u8] {
    let decl = crate::record::decl_token(decl);
    for t in [
        b"INT" as &[u8],
        b"INTEGER",
        b"REAL",
        b"TEXT",
        b"BLOB",
        b"ANY",
    ] {
        if eq_ci(decl, t) {
            return t;
        }
    }
    b""
}

/// A declared type as the pin shows it (v0.16 fix): dequoted as
/// `sqlite3AddColumn` keeps it (`record::decl_type_q`, doubled
/// closing quotes collapsed), and one of its standard type names
/// (`sqlite3StdType`: INT, INTEGER, REAL, TEXT, BLOB, ANY — any
/// case) in its own spelling.
fn shown_type(decl: &[u8]) -> AVec<u8> {
    match std_type(decl) {
        b"" => {
            let (d, close) = crate::record::decl_type_q(decl);
            let mut out = AVec::new();
            let mut prev_close = false;
            for c in d.iter() {
                if close != 0 && *c == close && prev_close {
                    prev_close = false;
                    continue;
                }
                prev_close = close != 0 && *c == close;
                out.push(*c);
            }
            out
        }
        t => copy_vec(t),
    }
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
        // An attached database's name reaches here only for what the
        // router leaves to main (v0.16, `attach`): a connection-wide
        // setting, `database_list`.
        #[cfg(feature = "attach")]
        let att = schema.is_some_and(|h| self.att_index(handle_bytes(arena, h)).is_some());
        #[cfg(not(feature = "attach"))]
        let att = false;
        if let Some(h) = schema {
            let b = handle_bytes(arena, h);
            if eq_ci(b, b"temp") {
                temp = true;
            } else if !eq_ci(b, b"main") && !att {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownDatabase,
                });
            }
        }
        let prag = lookup::<S::Error>(handle_bytes(arena, name))?;
        if att && prag.is_some_and(|p| !p.connection_wide() && p != Prag::DatabaseList) {
            return Err(Error::Unsupported);
        }
        // hipp has no temp database: a known pragma against it
        // is refused (an unknown one is still ignored).
        if temp && prag.is_some_and(|p| !p.connection_wide()) {
            return Err(Error::Unsupported);
        }
        let arg = value.map(|h| copy_vec(handle_bytes(arena, h)));
        // The pin refuses a safety-level change inside a transaction
        // when the statement is prepared (v0.16 fix).
        if prag == Some(Prag::Synchronous) && arg.is_some() && self.explicit_txn {
            return Err(Error::Schema {
                kind: SchemaKind::SafetyLevelInTransaction,
            });
        }
        // A table name — or a number outside 32 bits, which the pin
        // also reads as one (v0.16 fix) — must name a table.
        if let (Some(Prag::IntegrityCheck | Prag::QuickCheck), Some(a)) = (prag, &arg)
            && (!numeric
                || get_int32(a.as_slice().strip_prefix(b"-").unwrap_or(a.as_slice())).is_none())
            && self
                .catalog
                .find_readable(&self.catalog_arena, a.as_slice())
                .is_none()
        {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            });
        }
        // `foreign_key_check(t)` must name a table (the pin's "no such
        // table").
        #[cfg(feature = "foreign_keys")]
        if let (Some(Prag::ForeignKeyCheck), Some(a)) = (prag, &arg)
            && self
                .catalog
                .find(&self.catalog_arena, a.as_slice())
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
            // The connection's mode (v0.16, `autovacuum`: the pin's
            // in-memory one, which a ROLLBACK keeps).
            #[cfg(feature = "autovacuum")]
            Prag::AutoVacuum => row1(int(i64::from(self.pager.av_mode()))),
            #[cfg(not(feature = "autovacuum"))]
            Prag::AutoVacuum => row1(int(i64::from(h.auto_vacuum))),
            Prag::Encoding => row1(text(match self.pager.text_enc() {
                crate::record::TextEnc::Utf8 => b"UTF-8",
                #[cfg(feature = "utf16")]
                crate::record::TextEnc::Utf16le => b"UTF-16le",
                #[cfg(feature = "utf16")]
                crate::record::TextEnc::Utf16be => b"UTF-16be",
            })),
            // An in-memory database's mode is `memory` (v0.16 fix:
            // hipp's `:memory:` reported `delete`).
            Prag::JournalMode if self.pager.storage().is_memory() => row1(text(b"memory")),
            #[cfg(not(feature = "wal"))]
            Prag::JournalMode => row1(text(b"delete")),
            #[cfg(feature = "wal")]
            Prag::JournalMode => row1(text(if self.pager.wal_mode() {
                b"wal"
            } else {
                b"delete"
            })),
            // hipp syncs the journal before any page moves and
            // the file before the journal goes: FULL.
            #[cfg(not(feature = "wal"))]
            Prag::Synchronous => row1(int(2)),
            // The safety level `PRAGMA synchronous` set (v0.16, `wal`).
            #[cfg(feature = "wal")]
            Prag::Synchronous => row1(int(i64::from(self.pager.wal_cfg.sync) - 1)),
            #[cfg(feature = "wal")]
            Prag::JournalSizeLimit => row1(int(self.pager.wal_cfg.size_limit)),
            #[cfg(feature = "wal")]
            Prag::WalAutocheckpoint => row1(int(i64::from(self.pager.wal_cfg.autocheckpoint))),
            // A checkpoint is run by the statement (`step_pragma`).
            #[cfg(feature = "wal")]
            Prag::WalCheckpoint => return Err(Error::Unsupported),
            // Run by the statement (`run_pragma`).
            #[cfg(feature = "autovacuum")]
            Prag::IncrementalVacuum => return Err(Error::Unsupported),
            Prag::CacheSize => row1(int(self.cfg.cache_pages as i64)),
            Prag::BusyTimeout => row1(int(i64::from(self.prag.busy_timeout))),
            Prag::ForeignKeys => row1(int(i64::from(self.prag.foreign_keys))),
            #[cfg(feature = "foreign_keys")]
            Prag::DeferForeignKeys => row1(int(i64::from(self.fk.defer))),
            // Reads the file (`Statement::fk_check`): not as a
            // table-valued function.
            #[cfg(feature = "foreign_keys")]
            Prag::ForeignKeyCheck => return Err(Error::Unsupported),
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
                if self.temp.is_some() {
                    push_row(&mut rows, &[int(1), text(b"temp"), text(b"")]);
                }
                // The attached databases, numbered from 2 (v0.16,
                // `attach`).
                #[cfg(feature = "attach")]
                for (i, a) in self.attached.iter().enumerate() {
                    push_row(
                        &mut rows,
                        &[
                            int(i as i64 + 2),
                            text(a.name.as_slice()),
                            text(a.conn.pager.storage().file_name()),
                        ],
                    );
                }
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
                    #[allow(clippy::type_complexity)]
                    let (name, shown, ty, ncol, strict, wr): (
                        &[u8],
                        &[u8],
                        &[u8],
                        usize,
                        bool,
                        bool,
                    ) = match o {
                        Obj::Table(t) => {
                            let n = handle_bytes(ca, t.name);
                            (n, n, b"table", t.columns.len(), t.strict, !t.has_rowid())
                        }
                        Obj::View(v) => {
                            // The planned columns (v0.16 fix: the
                            // catalog's list is empty for a view
                            // hipp did not derive at load); none
                            // for one that cannot be planned, as
                            // on the pin.
                            let n = handle_bytes(ca, v.name);
                            let ncol = self.view_columns(n).map_or(0, |c| c.len());
                            (n, n, b"view", ncol, false, false)
                        }
                        Obj::Schema => (
                            b"sqlite_master",
                            b"sqlite_schema",
                            b"table",
                            5,
                            false,
                            false,
                        ),
                    };
                    if arg.is_some_and(|a| !eq_ci(a, name)) {
                        continue;
                    }
                    // An attached database lists itself by its name
                    // (v0.16, `attach`).
                    #[cfg(feature = "attach")]
                    let label = self.schema_label();
                    #[cfg(not(feature = "attach"))]
                    let label = b"main";
                    push_row(
                        &mut rows,
                        &[
                            text(label),
                            text(shown),
                            text(ty),
                            int(ncol as i64),
                            int(i64::from(wr)),
                            int(i64::from(strict)),
                        ],
                    );
                }
                #[cfg(feature = "attach")]
                let main_only = p.main_only || self.alias.is_some();
                #[cfg(not(feature = "attach"))]
                let main_only = p.main_only;
                // The TEMP database's objects (v0.16), as `temp`.
                if !main_only && let Some(tc) = &self.temp {
                    let tp = PragmaPlan {
                        prag: Some(Prag::TableList),
                        arg: p.arg.clone(),
                        numeric: false,
                        main_only: true,
                    };
                    let trows = tc.pragma_rows(arena, &tp).map_err(temp::mem_err)?;
                    for r in trows.iter() {
                        if matches!(r.as_slice().get(1), Some(OwnedValue::Text(n)) if n.as_slice() == b"sqlite_schema")
                        {
                            continue;
                        }
                        let mut r = r.clone();
                        if let Some(v) = r.as_mut_slice().first_mut() {
                            *v = text(b"temp");
                        }
                        rows.push(r);
                    }
                }
                if !main_only && arg.is_none_or(|a| eq_ci(a, b"sqlite_temp_master")) {
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
                // Then each attached database's, under its name (v0.16,
                // `attach`).
                #[cfg(feature = "attach")]
                if !main_only {
                    let ap = PragmaPlan {
                        prag: Some(Prag::TableList),
                        arg: p.arg.clone(),
                        numeric: false,
                        main_only: true,
                    };
                    for a in self.attached.iter() {
                        for r in a.conn.pragma_rows(arena, &ap)?.iter() {
                            rows.push(r.clone());
                        }
                    }
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
            } = crate::sql::parse::<S::Error>(handle_bytes(ca, t.sql), &sc, self.cfg.limits())?
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
                r.push(text(shown_type(handle_bytes(&sc, c.decl)).as_slice()));
                // The catalog's flag: a WITHOUT ROWID table's key
                // columns are NOT NULL too (v0.16).
                let nn = t
                    .columns
                    .as_slice()
                    .get(i)
                    .map_or(c.not_null, |tc| tc.not_null);
                r.push(int(i64::from(nn)));
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
            // The view's columns as planned (v0.16: views execute);
            // one that cannot be planned is the planner's error, as
            // on the pin ("no such table: ...").
            let names = self.view_names(arena, v)?;
            let mut types = self.view_types(arena, v);
            if types.len() != names.len() {
                // A column list of another length than the SELECT
                // (planned anyway, as on the pin): no types.
                types = AVec::new();
            }
            for (i, c) in names.iter().enumerate() {
                let ty = types.as_slice().get(i).cloned().unwrap_or_default();
                let mut r = AVec::new();
                r.push(int(i as i64));
                r.push(text(c.as_slice()));
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
    /// view's SELECT is planned with the views and CTEs it reads
    /// expanded as derived tables (`Connection::expand_view`); a
    /// SELECT hipp cannot plan gives no types.
    fn view_types(&self, arena: &Arena, v: &crate::schema::ViewDef) -> AVec<AVec<u8>> {
        let mut out = AVec::new();
        let ckpt = arena.checkpoint();
        let r = (|| -> Result<AVec<AVec<u8>>, S::Error> {
            let (select, _) = self.expand_view(arena, v)?;
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
                let aff = item.and_then(|e| item_aff(&plan, e)).map(|a| {
                    crate::rules::compound_affinity(
                        a,
                        item,
                        plan.compound.iter().map(|(_, t)| &**t),
                        i,
                    )
                });
                let z = item.and_then(|e| self.ztype(&plan, e));
                let ty: AVec<u8> = match (z, aff) {
                    (Some(z), Some(a)) if crate::record::affinity_of(z.as_slice()) == a => {
                        shown_type(z.as_slice())
                    }
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
                        // A table-valued function's columns have no
                        // declared type (v0.16, `json`).
                        #[cfg(feature = "json")]
                        if sp.tvf != 0 {
                            return None;
                        }
                        if let Some(d) = &sp.derived {
                            let it = d.items.as_slice().get(k).or_else(|| {
                                d.values
                                    .as_slice()
                                    .first()
                                    .and_then(|r| r.as_slice().get(k))
                            })?;
                            return self.ztype(d, it);
                        }
                        #[cfg(feature = "attach")]
                        if sp.ext != 0 {
                            let decl = self.ext_col(sp.ext, k)?.1;
                            return if decl.is_empty() { None } else { Some(decl) };
                        }
                        let (cat, ca) = match self.temp.as_deref() {
                            Some(tc) if sp.temp => (&tc.catalog, &tc.catalog_arena),
                            _ => (&self.catalog, &self.catalog_arena),
                        };
                        let t = cat.tables.iter().find(|t| t.root == sp.root)?;
                        let decl = handle_bytes(ca, t.columns.as_slice().get(k)?.decl);
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
        let ix = self.catalog.find_index(ca, name);
        // No index of that name: a WITHOUT ROWID table's name
        // describes its PRIMARY KEY index (v0.16), as on the pin.
        #[cfg(feature = "without_rowid")]
        let ix = ix.or_else(|| {
            let t = self.catalog.find(ca, name).filter(|t| !t.has_rowid())?;
            self.catalog
                .indexes_for(ca, handle_bytes(ca, t.name))
                .find(|i| i.clustered)
        });
        let Some(ix) = ix else {
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
            // An expression column (v0.16, `expr_index`): cid -2,
            // no name, as on the pin.
            #[cfg(feature = "expr_index")]
            let col = match &ix.ext {
                Some(x) if x.exprs.as_slice().get(i).is_some_and(|e| e.is_some()) => {
                    Some(usize::MAX)
                }
                _ => col,
            };
            let (cid, cname) = match col {
                #[cfg(feature = "expr_index")]
                Some(usize::MAX) => (-2, OwnedValue::Null),
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
        // A WITHOUT ROWID table's index ends with columns, not the
        // rowid (v0.16): the clustered key with every other stored
        // column (BINARY), a secondary index with the key columns
        // it does not hold.
        #[cfg(feature = "without_rowid")]
        if !t.has_rowid() {
            if x {
                let col_row = |rows: &mut Rows, seq: usize, k: usize, desc: bool, coll| {
                    let cname = t
                        .columns
                        .as_slice()
                        .get(k)
                        .map_or(OwnedValue::Null, |c| text(handle_bytes(ca, c.name)));
                    push_row(
                        rows,
                        &[
                            int(seq as i64),
                            int(k as i64),
                            cname,
                            int(i64::from(desc)),
                            text(coll_name(coll)),
                            int(0),
                        ],
                    );
                };
                let mut seq = n;
                if ix.clustered {
                    for (k, c) in t.columns.iter().enumerate() {
                        let virt = c.gen_expr.is_some() && !c.stored;
                        if virt || ix.columns.iter().any(|kc| kc.col == Some(k)) {
                            continue;
                        }
                        col_row(&mut rows, seq, k, false, crate::record::Coll::Binary);
                        seq += 1;
                    }
                } else if let Some(pk) = self
                    .catalog
                    .indexes_for(ca, handle_bytes(ca, t.name))
                    .find(|i| i.clustered)
                {
                    for pc in pk.columns.iter() {
                        let held = ix.columns.iter().enumerate().any(|(i, kc)| {
                            #[cfg(feature = "expr_index")]
                            if ix.ext.as_ref().is_some_and(|e| {
                                e.exprs.as_slice().get(i).is_some_and(|x| x.is_some())
                            }) {
                                return false;
                            }
                            let _ = i;
                            kc.col == pc.col && kc.coll == pc.coll
                        });
                        if held {
                            continue;
                        }
                        // A UNIQUE constraint's index appends them
                        // ascending (the pin's `bAscKeyBug`).
                        let desc = pc.desc && ix.origin != crate::schema::IndexOrigin::Auto;
                        if let Some(k) = pc.col {
                            col_row(&mut rows, seq, k, desc, pc.coll);
                            seq += 1;
                        }
                    }
                }
            }
            return Ok(rows);
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
            crate::sql::parse::<S::Error>(handle_bytes(ca, t.sql), &sc, self.cfg.limits())?
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
    /// to it is refused in a build without `foreign_keys`, which
    /// does not enforce them.
    #[cfg(not(feature = "foreign_keys"))]
    pub(super) fn fk_guard(&self, name: &[u8]) -> Result<(), S::Error> {
        if !self.prag.foreign_keys {
            return Ok(());
        }
        let ca = &self.catalog_arena;
        let sc = scratch();
        for t in self.catalog.tables.iter() {
            sc.reset();
            let Ok(Ast::CreateTable { fkeys, .. }) =
                crate::sql::parse::<S::Error>(handle_bytes(ca, t.sql), &sc, self.cfg.limits())
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
        // The empty temp schema — or, on the temp database itself
        // (a TEMP view's stored text), its own schema table.
        if eq_ci(name, b"sqlite_temp_schema") || eq_ci(name, b"sqlite_temp_master") {
            if args.is_some() {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            }
            if self.is_temp {
                let sql = b"SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema";
                return self.select_of(arena, sql).map(Some);
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
            // (The pin has no `pragma_wal_checkpoint` /
            // `pragma_wal_autocheckpoint`, v0.16 `wal`.)
            Some((_, p)) if *p != Prag::CaseSensitiveLike && !p.no_table() => *p,
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
        // The pin's hidden columns (v0.16 fix): `arg` for a pragma
        // that takes a name, `schema` for one that reads a database
        // (not table_list); more arguments are its "too many
        // arguments on pragma_x() - max N".
        let has_arg = prag.takes_name();
        let has_schema = prag != Prag::TableList
            && (has_arg
                || matches!(
                    prag,
                    Prag::AutoVacuum
                        | Prag::CacheSize
                        | Prag::JournalMode
                        | Prag::PageCount
                        | Prag::PageSize
                        | Prag::Synchronous
                )
                || prag.sized_journal());
        if vals.len() > usize::from(has_arg) + usize::from(has_schema) {
            return Err(Error::Schema {
                kind: SchemaKind::WrongArgumentCount,
            });
        }
        let arg = vals
            .as_slice()
            .first()
            .filter(|_| has_arg)
            .cloned()
            .flatten();
        let schema = vals
            .as_slice()
            .get(usize::from(has_arg))
            .filter(|_| has_schema)
            .cloned()
            .flatten();
        // An attached database named by the schema argument, or —
        // without one — holding the object (temp, main, then attached
        // in order) answers (v0.16, `attach`).
        #[cfg(feature = "attach")]
        {
            let att = match (&schema, &arg) {
                (Some(s), _) => self.att_index(s.as_slice()),
                (None, Some(a)) if prag != Prag::TableList && !self.attached.is_empty() => {
                    let elsewhere = self.temp_has(a.as_slice(), false)
                        || self.temp_has(a.as_slice(), true)
                        || self.main_has(a.as_slice(), false)
                        || self.main_has(a.as_slice(), true)
                        || self
                            .catalog
                            .find_view(&self.catalog_arena, a.as_slice())
                            .is_some();
                    if elsewhere {
                        None
                    } else {
                        self.attached.iter().position(|x| {
                            let ca = &x.conn.catalog_arena;
                            x.conn.catalog.taken(ca, a.as_slice()).is_some()
                        })
                    }
                }
                _ => None,
            };
            if let Some(i) = att
                && let Some(x) = self.attached.as_slice().get(i)
            {
                let mut a2 = AVec::new();
                if let Some(a) = args {
                    for (k, e) in a.iter().enumerate() {
                        if k < usize::from(has_arg) {
                            a2.push(e.clone());
                        }
                    }
                }
                return x.conn.pragma_source(arena, name, Some(&a2));
            }
        }
        // A schema other than main: the temp database holds no
        // objects in hipp (`sqlite_temp_schema` is empty), but its
        // header values are not hipp's to report (as for `PRAGMA
        // temp.x`); any other name is unknown.
        let temp = schema
            .as_ref()
            .is_some_and(|s| eq_ci(s.as_slice(), b"temp"));
        if let Some(s) = &schema {
            if temp && !has_arg {
                return Err(Error::Unsupported);
            }
            if !temp && !eq_ci(s.as_slice(), b"main") {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownDatabase,
                });
            }
        }
        // v0.16: a name the TEMP database has (temp first, as on the
        // pin), or the `temp` schema: that database's rows.
        let in_temp = has_arg
            && prag != Prag::TableList
            && !self.is_temp
            && match (&schema, &arg) {
                (Some(_), Some(_)) => temp,
                (None, Some(a)) => {
                    self.temp_has(a.as_slice(), false) || self.temp_has(a.as_slice(), true)
                }
                _ => false,
            };
        let rows = if in_temp {
            match self.temp.as_deref() {
                Some(tc) => tc
                    .pragma_rows(
                        arena,
                        &PragmaPlan {
                            prag: Some(prag),
                            arg,
                            numeric: false,
                            main_only: false,
                        },
                    )
                    .map_err(super::temp::mem_err)?,
                None => AVec::new(),
            }
        } else if temp || (has_arg && prag != Prag::TableList && arg.is_none()) {
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
                    sql_literal(v, &mut sql, self.pager.text_enc());
                }
                sql.push(b')');
            }
            sql.push(b')');
        }
        self.select_of(arena, sql.as_slice()).map(Some)
    }

    fn select_of(&self, arena: &Arena, sql: &[u8]) -> Result<SelectStmt, S::Error> {
        match crate::sql::parse::<S::Error>(sql, arena, self.cfg.limits())? {
            Ast::Select(s) => Ok(s),
            _ => Err(Error::Unsupported),
        }
    }
}

/// An SQL literal for a pragma value: integers in decimal, text
/// as `CAST(x'..' AS TEXT)` (any bytes survive; in a UTF-16
/// database, v0.16 `utf16`, the blob holds the text's UTF-16 bytes,
/// which that CAST reads back), NULL.
fn sql_literal(v: &OwnedValue, out: &mut AVec<u8>, enc: crate::record::TextEnc) {
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
            #[cfg(feature = "utf16")]
            let t = &if blob {
                t.clone()
            } else {
                crate::utf16::encode(t.as_slice(), enc)
            };
            let _ = enc;
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
/// `view_aff` looking through a derived table's (an expanded
/// view's or CTE's) column to the item it reads (v0.16): such a
/// column has its item's affinity, none for a literal — where the
/// derived source's comparison affinity says BLOB.
fn item_aff(plan: &crate::query::SubPlan, e: &RExpr) -> Option<Affinity> {
    match e {
        RExpr::Collate(x, _) => item_aff(plan, x),
        RExpr::Column(slot, ..) => {
            let mut base = 0usize;
            for sp in plan.sources.iter() {
                if let Some(k) = slot.checked_sub(base).filter(|k| *k < sp.ncols) {
                    #[cfg(feature = "json")]
                    if sp.tvf != 0 {
                        return None;
                    }
                    if let Some(d) = &sp.derived {
                        let it = d.items.as_slice().get(k).or_else(|| {
                            d.values
                                .as_slice()
                                .first()
                                .and_then(|r| r.as_slice().get(k))
                        })?;
                        return item_aff(d, it);
                    }
                    break;
                }
                base = base.saturating_add(sp.ncols);
            }
            view_aff(e)
        }
        _ => view_aff(e),
    }
}

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

    /// `PRAGMA auto_vacuum = mode` (v0.16, `autovacuum`), as on the
    /// pin: the mode is always kept for the next VACUUM; an empty
    /// database takes it at once (its header written, as the pin's
    /// pragma creates the file); an auto-vacuum file switches between
    /// FULL and INCREMENTAL (header offset 64); a change to or from
    /// NONE otherwise waits for VACUUM.
    #[cfg(feature = "autovacuum")]
    async fn set_auto_vacuum(&mut self, want: u8, pristine: bool) -> Result<Rows, S::Error> {
        let cur = self.conn.pager.av_mode();
        if !pristine {
            self.conn.prag.vacuum_autovac = Some(want);
        }
        if want == cur {
            return Ok(AVec::new());
        }
        if pristine && want != 0 {
            crate::autovac::set_mode(&mut self.conn.pager, want).await?;
        } else if !pristine && want != 0 && cur != 0 {
            let pager = &mut self.conn.pager;
            pager.set_header_u32(64, u32::from(want == 2)).await?;
            pager.set_av_mode(want);
            pager.header_mut().auto_vacuum = want;
        } else {
            return Ok(AVec::new());
        }
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(AVec::new())
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
        #[cfg(feature = "foreign_keys")]
        if prag == Prag::ForeignKeyCheck {
            return self.fk_check(&p).await;
        }
        #[cfg(feature = "wal")]
        if prag == Prag::WalCheckpoint {
            return self
                .wal_checkpoint(p.arg.as_ref().map(|a| a.as_slice()))
                .await;
        }
        // v0.16 (`autovacuum`): one empty row per page removed; a
        // limit that is not a positive 32-bit integer (or none) is
        // every page, as the pin reads it (`sqlite3GetInt32`).
        #[cfg(feature = "autovacuum")]
        if prag == Prag::IncrementalVacuum {
            let limit = match p.arg.as_ref().and_then(|a| get_int32(a.as_slice())) {
                Some(n) if n > 0 => n as u32,
                _ => i32::MAX as u32,
            };
            let steps = crate::autovac::incremental(&mut self.conn.pager, limit).await?;
            if self.conn.pager.in_txn() && !self.conn.explicit_txn {
                self.conn.pager.drive_commit().await?;
            }
            let mut rows: Rows = AVec::new();
            for _ in 0..steps {
                rows.push(AVec::new());
            }
            return Ok(rows);
        }
        let Some(arg) = p.arg.as_ref().map(|a| copy_vec(a.as_slice())) else {
            return self.conn.pragma_rows(&self.arena, &p);
        };
        let arg = arg.as_slice();
        let pristine = self.conn.pager.layout_open()
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
                } else if ok && !pristine {
                    // Otherwise the pin records it for the next
                    // VACUUM (a bad size is ignored).
                    self.conn.prag.vacuum_page_size = Some(n as u32);
                }
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
                // v0.16 (`utf16`): the pin's encoding stays open
                // while the database has no schema and neither it nor
                // the TEMP database has had a schema change since the
                // open (`Pager::enc_open`).
                #[cfg(feature = "utf16")]
                let pristine = self.conn.pager.enc_open()
                    && self.conn.catalog.tables.is_empty()
                    && self.conn.catalog.views.is_empty()
                    && !self
                        .conn
                        .temp
                        .as_deref()
                        .is_some_and(|t| t.pager.schema_changed());
                if !pristine {
                    // Fixed once the database has content: ignored.
                    return Ok(AVec::new());
                }
                match NAMES.iter().position(|(n, _)| eq_ci(n.as_bytes(), arg)) {
                    #[cfg(feature = "utf16")]
                    Some(i) => {
                        use crate::record::TextEnc;
                        // `UTF-16` alone is the native order; hipp
                        // takes the pin's (x86-64): little-endian.
                        let enc = match i {
                            0 | 1 => TextEnc::Utf8,
                            3 | 5 => TextEnc::Utf16be,
                            _ => TextEnc::Utf16le,
                        };
                        self.conn.set_text_enc(enc);
                        Ok(AVec::new())
                    }
                    #[cfg(not(feature = "utf16"))]
                    Some(i) if NAMES.get(i).is_some_and(|n| n.1) => Ok(AVec::new()),
                    #[cfg(not(feature = "utf16"))]
                    Some(_) => Err(Error::UnsupportedEncoding),
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
                #[cfg(feature = "autovacuum")]
                return self.set_auto_vacuum(want, pristine).await;
                #[cfg(not(feature = "autovacuum"))]
                let cur = self.conn.pager.header().auto_vacuum;
                #[cfg(not(feature = "autovacuum"))]
                if want == cur || (!pristine && (want == 0 || cur == 0)) {
                    // Unchanged, or (as on the pin) a change that
                    // waits for the next VACUUM.
                    if !pristine {
                        self.conn.prag.vacuum_autovac = Some(want);
                    }
                    Ok(AVec::new())
                } else {
                    // hipp keeps no pointer maps without `autovacuum`.
                    Err(Error::Unsupported)
                }
            }
            Prag::JournalMode => {
                const MODES: [&str; 6] = ["delete", "persist", "off", "truncate", "memory", "wal"];
                // The pin takes the first mode the value is a prefix
                // of, ignoring case (`w` is WAL, `''` DELETE; v0.16
                // fix: hipp compared whole names).
                let mode = MODES.iter().position(|m| {
                    m.len() >= arg.len()
                        && eq_ci(m.as_bytes().get(..arg.len()).unwrap_or_default(), arg)
                });
                match mode {
                    // An unknown mode queries the current one; an
                    // in-memory database keeps `memory` whatever is
                    // asked, as on the pin (v0.16 fix: hipp's
                    // `:memory:` entered WAL mode).
                    None => self.conn.pragma_rows(&self.arena, &p_query(&p)),
                    Some(_) if self.conn.pager.storage().is_memory() => {
                        self.conn.pragma_rows(&self.arena, &p_query(&p))
                    }
                    #[cfg(feature = "wal")]
                    Some(m @ (0 | 5)) => self.set_journal_mode(m == 5, &p).await,
                    #[cfg(not(feature = "wal"))]
                    Some(0) => Ok(row1(text(b"delete"))),
                    Some(_) => Err(Error::Unsupported),
                }
            }
            #[cfg(not(feature = "wal"))]
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
            // Any safety level (v0.16, `wal`): OFF skips every sync,
            // NORMAL syncs a WAL-mode log only at checkpoints, FULL
            // and EXTRA at every commit (`pager::WalCfg::sync`).
            #[cfg(feature = "wal")]
            Prag::Synchronous => {
                let level = (safety_level(arg, false, 1).wrapping_add(1)) & 7;
                self.conn.pager.wal_cfg.sync = if level == 0 { 1 } else { level };
                Ok(AVec::new())
            }
            #[cfg(feature = "wal")]
            Prag::WalAutocheckpoint => {
                self.conn.pager.wal_cfg.autocheckpoint = atoi(arg).max(0) as u32;
                self.conn.pragma_rows(&self.arena, &p)
            }
            #[cfg(feature = "wal")]
            Prag::JournalSizeLimit => {
                self.conn.pager.wal_cfg.size_limit = atoi64(arg).max(-1);
                self.conn.pragma_rows(&self.arena, &p)
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
            #[cfg(feature = "foreign_keys")]
            Prag::DeferForeignKeys => {
                self.conn.fk.defer = boolean(arg);
                Ok(AVec::new())
            }
            Prag::IgnoreCheckConstraints => {
                self.conn.prag.ignore_checks = boolean(arg);
                Ok(AVec::new())
            }
            Prag::CaseSensitiveLike => {
                self.conn.env.case_like.set(boolean(arg));
                self.conn.prag.like_redefined = true;
                Ok(AVec::new())
            }
            _ => self.conn.pragma_rows(&self.arena, &p),
        }
    }

    /// `foreign_key_check [table]` (v0.16, `foreign_keys`): every
    /// child row whose parent key is missing — `table, rowid,
    /// parent, fkid` — table by table (the pin's hash order, see
    /// `hash_order`), rows in rowid order, keys in list order. Every
    /// key is resolved first: a mismatch fails the pragma.
    #[cfg(feature = "foreign_keys")]
    async fn fk_check(&mut self, p: &PragmaPlan) -> Result<Rows, S::Error> {
        let mut names: AVec<AVec<u8>> = AVec::new();
        {
            let ca = &self.conn.catalog_arena;
            match &p.arg {
                Some(a) => {
                    let t = self
                        .conn
                        .catalog
                        .find(ca, a.as_slice())
                        .ok_or(Error::Schema {
                            kind: SchemaKind::UnknownTable,
                        })?;
                    names.push(copy_vec(handle_bytes(ca, t.name)));
                }
                None => {
                    for o in self.conn.hash_order().iter() {
                        if let Obj::Table(t) = o {
                            names.push(copy_vec(handle_bytes(ca, t.name)));
                        }
                    }
                }
            }
        }
        let plan = self.conn.fk_check_plan(names.as_slice(), &self.arena)?;
        let found = super::fkey::fk_check_rows(&mut self.conn.pager, plan.as_slice()).await?;
        let mut rows = AVec::new();
        for (t, rowid, parent, id) in found.iter() {
            push_row(
                &mut rows,
                &[
                    text(t.as_slice()),
                    int(*rowid),
                    text(parent.as_slice()),
                    int(*id as i64),
                ],
            );
        }
        Ok(rows)
    }

    /// `integrity_check` / `quick_check` (see the module docs):
    /// the pin's result rows, limit and order.
    async fn integrity(&mut self, p: &PragmaPlan, quick: bool) -> Result<Rows, S::Error> {
        // The limit: a number argument that is a 32-bit integer
        // (the pin reads the token without its sign), else 100; a
        // name argument — or a number outside 32 bits, sign
        // included (v0.16 fix) — checks one table.
        let mut mx: i64 = 100;
        let mut only: Option<AVec<u8>> = None;
        if let Some(a) = &p.arg {
            let s = a.as_slice();
            match get_int32(s.strip_prefix(b"-").unwrap_or(s)) {
                Some(n) if p.numeric => mx = if n <= 0 { 100 } else { i64::from(n) },
                _ => only = Some(a.clone()),
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
                    // A WITHOUT ROWID table's key is the table's own
                    // tree (v0.16), checked as the table.
                    #[cfg(feature = "without_rowid")]
                    if ix.clustered {
                        continue;
                    }
                    // The full check computes each row's key in
                    // every index; hipp cannot for an expression or
                    // partial index, so it refuses before reporting
                    // anything (v0.16 fix: it printed a false
                    // "wrong # of entries" or "ok", then failed).
                    if ix.opaque && !quick {
                        return Err(Error::Unsupported);
                    }
                    let partial = ix
                        .sql
                        .is_some_and(|h| sql_has_where::<S::Error>(handle_bytes(ca, h)));
                    ixs.push((copy_vec(handle_bytes(ca, ix.name)), ix.root, partial));
                }
                tables.push((copy_vec(name), root, ixs));
            }
        }
        let mut roots: AVec<u32> = AVec::new();
        for (_, r, ixs) in tables.iter() {
            roots.push(*r);
            for (_, ir, _) in ixs.iter() {
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
            return Ok(out.finish(self.deferred.is_some()));
        }
        // 2. Index entry counts against the table's (a partial
        // index holds only some rows: the pin skips it here).
        let mut k = 0usize;
        for (_, _, ixs) in tables.iter() {
            let tc = counts.as_slice().get(k).copied();
            k += 1;
            for (iname, _, partial) in ixs.iter() {
                let ic = counts.as_slice().get(k).copied();
                k += 1;
                if tc != ic && !*partial && !out.stop {
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
        Ok(out.finish(self.deferred.is_some()))
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
            let mut v = self.conn.write_index_plans(&scratch(), tname)?;
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
        // A WITHOUT ROWID table's key is its own tree (v0.16): no
        // entry to look for.
        #[cfg(feature = "without_rowid")]
        let (indexes, inames) = {
            let mut a = AVec::new();
            let mut b = AVec::new();
            for (ix, n) in indexes.iter().zip(inames.iter()) {
                if ix.root != root {
                    a.push(ix.clone());
                    b.push(n.clone());
                }
            }
            (a, b)
        };
        #[cfg(not(feature = "indexes"))]
        let _ = quick;
        // Nothing to check row by row: the pin then reads only
        // each record's header, so hipp reads only the cells' local
        // bytes (a damaged overflow chain is the page check's
        // finding alone).
        #[cfg(feature = "indexes")]
        let no_index = indexes.is_empty();
        #[cfg(not(feature = "indexes"))]
        let no_index = true;
        let virt = rules.as_ref().and_then(|r| r.virt().map(copy_bools));
        // The pin reads each row's right-most stored column (not
        // the rowid alias, not a VIRTUAL one) first, which parses
        // and checks the record header up to it: `p2` is its index
        // in the record.
        let is_virt = |j: usize| {
            virt.as_ref()
                .is_some_and(|v| v.as_slice().get(j).copied().unwrap_or(false))
        };
        let p2 = (0..ncols)
            .rev()
            .find(|j| Some(*j) != alias && !is_virt(*j))
            .map(|m| (0..m).filter(|j| !is_virt(*j)).count());
        if no_index
            && !nn.iter().any(|b| *b)
            && strict.iter().all(|s| s.is_none())
            && rules.as_ref().is_none_or(|r| r.checks.is_empty())
        {
            if walk_pages(&mut self.conn.pager, root, p2).await? {
                out.malformed();
            }
            return Ok(());
        }
        let mut cursor = ScanCursor::new(root);
        let mut buf = AVec::new();
        // The pin's messages name a row by its scan ordinal.
        let mut nrow: i64 = 0;
        // Rows each index should hold (a partial index's count is
        // checked against these, after the rows).
        #[cfg(feature = "indexes")]
        let mut in_index: AVec<u64> = AVec::new();
        #[cfg(feature = "indexes")]
        for _ in indexes.iter() {
            in_index.push(0);
        }
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            if out.stop {
                return Ok(());
            }
            nrow += 1;
            if p2.is_some_and(|p| record_header_corrupt(buf.as_slice(), buf.len() as u64, p)) {
                out.malformed();
                return Ok(());
            }
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
            for (j, (ix, iname)) in indexes.iter().zip(inames.iter()).enumerate() {
                // v0.16 (`expr_index`): an expression index's key is
                // computed; a row a partial index's WHERE rejects
                // has no entry to find.
                let Some(key) = ix_key::<S::Error>(
                    ix,
                    rec.as_slice(),
                    rowid,
                    alias,
                    #[cfg(feature = "expr_index")]
                    &self.conn.env,
                )?
                else {
                    continue;
                };
                if let Some(n) = in_index.as_mut_slice().get_mut(j) {
                    *n += 1;
                }
                let unique = ix.unique && key_has_no_null(key.as_slice());
                let (found, dup) = btree::index_check_entry(
                    &mut self.conn.pager,
                    ix.root,
                    ix.desc.as_slice(),
                    key.as_slice(),
                    rowid,
                    unique,
                )
                .await?;
                if !found {
                    let mut s = copy_vec(b"row ");
                    dec(nrow, &mut s);
                    s.extend_from_slice(b" missing from index ");
                    s.extend_from_slice(iname.as_slice());
                    if !out.emit(s) {
                        return Ok(());
                    }
                } else if dup {
                    let mut s = copy_vec(b"non-unique entry in index ");
                    s.extend_from_slice(iname.as_slice());
                    if !out.emit(s) {
                        return Ok(());
                    }
                }
            }
        }
        let _ = nrow;
        // A partial index's entry count against the rows its WHERE
        // admits (v0.16, `expr_index`: the pin's per-index counter,
        // compared after the row loop).
        #[cfg(feature = "expr_index")]
        for ((ix, iname), want) in indexes.iter().zip(inames.iter()).zip(in_index.iter()) {
            if ix.ext.as_ref().is_none_or(|x| x.pred.is_none()) {
                continue;
            }
            let mut cur = btree::IndexCursor::new(ix.root, ix.desc.as_slice());
            let mut have = 0u64;
            while cur.next_entry(&mut self.conn.pager).await?.is_some() {
                have += 1;
            }
            if have != *want {
                let mut m = copy_vec(b"wrong # of entries in index ");
                m.extend_from_slice(iname.as_slice());
                if !out.emit(m) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// Visit a table tree's pages without reading its cells' overflow
/// chains (the pin's header-only pass over a table with nothing to
/// check per row): a page that is not a B-tree page, or a child
/// outside the file, is the corruption error the pin's scan meets.
/// True when a leaf cell's record header fails the pin's check for
/// record column `p2` (`record_header_corrupt`, on the local bytes;
/// a header that runs into an overflow page is not checked).
async fn walk_pages<S: Storage>(
    pager: &mut Pager<S>,
    root: u32,
    p2: Option<usize>,
) -> Result<bool, S::Error> {
    let mut stack: AVec<u32> = AVec::new();
    stack.push(root);
    let mut budget = pager.page_count();
    let usable = pager.usable() as usize;
    // Leaves are visited in reverse order here; the pin reports the
    // first bad row in scan order — but the message is the same
    // whichever row it is, and the check stops at it.
    while let Some(page) = stack.pop() {
        if budget == 0 {
            return Err(Error::CorruptPage { page });
        }
        budget -= 1;
        let d = page_copy(pager, page).await?;
        let h = btree::head::<S::Error>(d.as_slice(), page)?;
        if h.is_leaf() {
            let Some(p2) = p2 else { continue };
            let d = d.as_slice();
            for i in 0..usize::from(h.ncell) {
                let at = h.cell_ptrs_off() + 2 * i;
                let Some(pc) = d.get(at..at + 2).map(|_| usize::from(be_u16(d, at))) else {
                    continue;
                };
                let Some(cell) = d.get(pc..) else { continue };
                let Ok((plen, n1)) = decode_varint::<()>(cell) else {
                    continue;
                };
                let Some(Ok((_, n2))) = cell.get(n1..).map(decode_varint::<()>) else {
                    continue;
                };
                let local =
                    usize::try_from(plen).map_or(usable, |p| btree::table_leaf_local(usable, p));
                let Some(body) = cell.get(n1 + n2..) else {
                    continue;
                };
                let body = body.get(..local).unwrap_or(body);
                if record_header_corrupt(body, plen, p2) {
                    return Ok(true);
                }
            }
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
    Ok(false)
}

/// The pin's `OP_Column` record-header test (v0.16 fix): reading
/// record column `p2` of a table row whose record bytes start
/// `rec` (all of them, or the cell's local part) and whose payload
/// is `payload` bytes. The integrity check reads each row's
/// right-most stored column, and the pin answers a failure with the
/// result row "database disk image is malformed" and stops. A
/// header size of 0 is accepted (all columns NULL), as on the pin;
/// a header whose bytes are not all at hand passes.
fn record_header_corrupt(rec: &[u8], payload: u64, p2: usize) -> bool {
    let Some((hdr, mut z)) = varint32(rec) else {
        return false;
    };
    if hdr > 98_307 || u64::from(hdr) > payload {
        return true;
    }
    let end = hdr as usize;
    let mut off = u64::from(hdr);
    let mut i = 0usize;
    loop {
        let Some((t, n)) = rec.get(z..).and_then(varint32) else {
            return false;
        };
        z += n;
        off = off.saturating_add(u64::from(serial_len(t)));
        i += 1;
        if i > p2 || z >= end {
            break;
        }
    }
    let bad = (z >= end && (z > end || off != payload)) || off > payload;
    bad && hdr != 0
}

/// The pin's `getVarint32`: a varint, saturated to 32 bits.
fn varint32(b: &[u8]) -> Option<(u32, usize)> {
    match b.first() {
        Some(c) if *c < 0x80 => Some((u32::from(*c), 1)),
        Some(_) => decode_varint::<()>(b)
            .ok()
            .map(|(v, n)| (u32::try_from(v).unwrap_or(u32::MAX), n)),
        None => None,
    }
}

/// A serial type's content length (`sqlite3VdbeSerialTypeLen`).
fn serial_len(t: u32) -> u32 {
    match t {
        0..=4 => t,
        5 => 6,
        6 | 7 => 8,
        8..=11 => 0,
        _ => (t - 12) / 2,
    }
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

    /// The pin's answer to a record it cannot read: one more row,
    /// "database disk image is malformed", and the check stops.
    fn malformed(&mut self) {
        if !self.stop {
            let mut r = AVec::new();
            r.push(text(b"database disk image is malformed"));
            self.rows.push(r);
        }
        self.stop = true;
    }

    /// The result rows: `ok` when nothing was reported, unless
    /// the check stops on an error (`deferred`), which follows the
    /// rows reported so far — none, then, as on the pin.
    fn finish(mut self, deferred: bool) -> Rows {
        if self.rows.is_empty() && !deferred {
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
    Page {
        root: u32,
        page: u32,
    },
    Cell {
        root: u32,
        page: u32,
        cell: u32,
    },
    /// "Tree R page P right child: " (an auto-vacuum file's
    /// right-child pointer-map check, and that child's own check).
    Right {
        root: u32,
        page: u32,
    },
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
    /// The file is auto-vacuum (header offset 52 nonzero): its
    /// pointer map is checked (v0.16 fix).
    autovac: bool,
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
            autovac: pager.header().auto_vacuum != 0,
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
            Pfx::Right { root, page } => {
                m.extend_from_slice(b"Tree ");
                dec(i64::from(root), m);
                m.extend_from_slice(b" page ");
                dec(i64::from(page), m);
                m.extend_from_slice(b" right child: ");
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

/// The pin's `ptrmapPageno`: the pointer-map page holding `page`'s
/// entry (0 below page 2; the lock-byte page is skipped).
fn ptrmap_page(ck: &Ck, page: u32) -> u32 {
    if page < 2 {
        return 0;
    }
    let per = (ck.usable / 5 + 1) as u32;
    let p = ((page - 2) / per) * per + 2;
    let pending = 1_073_741_824 / (ck.page_size as u32).max(1) + 1;
    if p == pending { p + 1 } else { p }
}

/// The pin's `checkPtrmap` (v0.16 fix: auto-vacuum pointer maps
/// were not checked): `child`'s entry must read `(kind, parent)`.
async fn check_ptrmap<S: Storage>(
    ck: &mut Ck,
    pager: &mut Pager<S>,
    child: u32,
    kind: u8,
    parent: u32,
    pfx: Pfx,
) -> Result<(), S::Error> {
    let pm = ptrmap_page(ck, child);
    let off = (child as usize).checked_sub(pm as usize + 1).map(|k| 5 * k);
    let got = match off {
        Some(off) if pm != 0 && pm <= ck.npage => {
            let d = page_copy(pager, pm).await?;
            d.as_slice()
                .get(off..off + 5)
                .map(|e| (e[0], be_u32(e, 1)))
                .filter(|(t, _)| (1..=5).contains(t))
        }
        _ => None,
    };
    match got {
        None => ck.msg_n(pfx, b"Failed to read ptrmap key=", i64::from(child), b""),
        Some((t, p)) if t != kind || p != parent => {
            let mut s = copy_vec(b"Bad ptr map entry key=");
            dec(i64::from(child), &mut s);
            s.extend_from_slice(b" expected=(");
            dec(i64::from(kind), &mut s);
            s.push(b',');
            dec(i64::from(parent), &mut s);
            s.extend_from_slice(b") got=(");
            dec(i64::from(t), &mut s);
            s.push(b',');
            dec(i64::from(p), &mut s);
            s.push(b')');
            ck.msg(pfx, s.as_slice());
        }
        Some(_) => {}
    }
    Ok(())
}

/// The pin's pointer-map entry kinds.
const PTRMAP_ROOTPAGE: u8 = 1;
const PTRMAP_FREEPAGE: u8 = 2;
const PTRMAP_OVERFLOW1: u8 = 3;
const PTRMAP_OVERFLOW2: u8 = 4;
const PTRMAP_BTREE: u8 = 5;

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
            if ck.autovac {
                check_ptrmap(ck, pager, page, PTRMAP_FREEPAGE, 0, pfx).await?;
            }
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
                        if ck.autovac {
                            check_ptrmap(ck, pager, leaf, PTRMAP_FREEPAGE, 0, pfx).await?;
                        }
                        ck.check_ref(leaf, pfx);
                    }
                }
                n = n.wrapping_sub(leaves);
            }
        } else if ck.autovac && n > 0 {
            // The next overflow page names this one as its parent.
            let next = d.as_slice().get(..4).map_or(0, |_| be_u32(d.as_slice(), 0));
            check_ptrmap(ck, pager, next, PTRMAP_OVERFLOW2, page, pfx).await?;
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
            let mut rpfx = Pfx::Cell {
                root,
                page,
                cell: v2,
            };
            if ck.autovac {
                rpfx = Pfx::Right { root, page };
                check_ptrmap(ck, pager, right, PTRMAP_BTREE, page, rpfx).await?;
            }
            let (dp, mk, c) = check_tree(ck, pager, root, right, v2, rpfx, max_key).await?;
            depth = dp;
            max_key = mk;
            count += c;
            key_eq = false;
        }
        // In an auto-vacuum file the pin's prefix stays at the
        // right child's for the rest of an interior page.
        let right_pfx = ck.autovac && !leaf;
        let mut i = ncell;
        while i > 0 && ck.mx > 0 {
            i -= 1;
            let cpfx = if right_pfx {
                Pfx::Right { root, page }
            } else {
                Pfx::Cell {
                    root,
                    page,
                    cell: i as u32,
                }
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
            // A table row's record header size must cover at least
            // its own byte (the pin flags a first byte of 0 or 1,
            // nothing larger: an oversized header is the row
            // check's "malformed" — v0.16 fix).
            if kind == btree::KIND_LEAF_TABLE && info.payload > 0 {
                let rec = pc + info.body;
                // (The pin tests the record's first byte: a two-byte
                // varint 0x80 0x00 passes.)
                if d.get(rec).is_none_or(|h| *h < 2) {
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
                if ck.autovac {
                    check_ptrmap(ck, pager, ovfl, PTRMAP_OVERFLOW1, page, cpfx).await?;
                }
                check_list(ck, pager, false, ovfl, npage as u32, cpfx).await?;
            }
            if !leaf {
                if ck.autovac {
                    check_ptrmap(ck, pager, info.child, PTRMAP_BTREE, page, cpfx).await?;
                }
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
            // The pin pulls the heap in order and stops at an
            // overlap; the fragmentation test still runs when that
            // overlap was the last entry (v0.16 fix: it was skipped).
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
                    break;
                }
                nfrag += *a as i64 - prev_end - 1;
                prev_end = *b as i64;
            }
            nfrag += usable as i64 - prev_end - 1;
            if emptied && nfrag != frag as i64 {
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
        // The header's auto-vacuum fields (v0.16 fix: unchecked).
        let p1 = page_copy(pager, 1).await?;
        let field = |o: usize| {
            p1.as_slice()
                .get(o..o + 4)
                .map_or(0, |_| be_u32(p1.as_slice(), o))
        };
        if ck.autovac {
            let mx = roots.iter().copied().max().unwrap_or(0);
            let hdr = field(52);
            if mx != hdr {
                let mut s = copy_vec(b"max rootpage (");
                dec(i64::from(mx), &mut s);
                s.extend_from_slice(b") disagrees with header (");
                dec(i64::from(hdr), &mut s);
                s.push(b')');
                ck.msg(Pfx::None, s.as_slice());
            }
        } else if field(64) != 0 {
            ck.msg(
                Pfx::None,
                b"incremental_vacuum enabled with a max rootpage of zero",
            );
        }
    }
    for r in roots {
        if ck.mx <= 0 {
            counts.push(0);
            continue;
        }
        if ck.autovac && *r > 1 && !partial {
            check_ptrmap(ck, pager, *r, PTRMAP_ROOTPAGE, 0, Pfx::None).await?;
        }
        let (_, _, c) = check_tree(ck, pager, *r, *r, 0, Pfx::None, i64::MAX).await?;
        counts.push(c);
    }
    if !partial {
        for i in 1..=ck.npage {
            if ck.mx <= 0 {
                break;
            }
            let ptrmap = ck.autovac && ptrmap_page(ck, i) == i;
            if !ck.is_seen(i) && !ptrmap {
                ck.msg_n(Pfx::None, b"Page ", i64::from(i), b": never used");
            }
            if ck.is_seen(i) && ptrmap {
                ck.msg_n(
                    Pfx::None,
                    b"Page ",
                    i64::from(i),
                    b": pointer map referenced",
                );
            }
        }
    }
    Ok(())
}

// ----------------------------------------------------------------
// WAL mode (v0.16, `wal`).
// ----------------------------------------------------------------

#[cfg(feature = "wal")]
impl<S: Storage> Statement<'_, S> {
    /// `PRAGMA journal_mode = WAL | DELETE` (v0.16, `wal`), as the
    /// pin's `OP_JournalMode` decides: no change once the open
    /// transaction has written (the current mode is reported), "cannot
    /// change into / out of wal mode from within a transaction" in an
    /// explicit one that has not, else the switch (`Pager::enter_wal`
    /// / `leave_wal`). Reports the mode in force after it.
    async fn set_journal_mode(&mut self, wal: bool, p: &PragmaPlan) -> Result<Rows, S::Error> {
        // The TEMP database never enters WAL mode (the pin's temporary
        // storage): the request reports its mode.
        if wal != self.conn.pager.wal_mode() && !self.conn.pager.in_txn() && !self.conn.is_temp {
            if self.conn.explicit_txn {
                return Err(Error::Schema {
                    kind: SchemaKind::WalModeInTransaction,
                });
            }
            if wal {
                self.conn.pager.enter_wal().await?;
            } else {
                self.conn.pager.leave_wal().await?;
            }
        }
        self.conn.pragma_rows(&self.arena, &p_query(p))
    }

    /// `PRAGMA wal_checkpoint [(PASSIVE | FULL | RESTART | TRUNCATE |
    /// NOOP)]` (v0.16, `wal`; any other value is PASSIVE): the pin's
    /// row `busy | log | checkpointed` — hipp is the file's only
    /// process, so never busy; `0 | -1 | -1` outside WAL mode. A
    /// transaction that has read or written the database refuses it
    /// (the pin's "database table is locked"), in either mode.
    async fn wal_checkpoint(&mut self, arg: Option<&[u8]>) -> Result<Rows, S::Error> {
        use crate::pager::Checkpoint;
        let is = |w: &[u8]| arg.is_some_and(|a| eq_ci(a, w));
        if self.conn.pager.in_txn() || (self.conn.explicit_txn && self.conn.pager.touched()) {
            return Err(Error::Schema {
                kind: SchemaKind::CheckpointLocked,
            });
        }
        let r = if is(b"noop") {
            self.conn.pager.wal_frames()
        } else {
            let mode = if is(b"full") {
                Checkpoint::Full
            } else if is(b"restart") {
                Checkpoint::Restart
            } else if is(b"truncate") {
                Checkpoint::Truncate
            } else {
                Checkpoint::Passive
            };
            self.conn.pager.checkpoint(mode).await?
        };
        self.conn.pager.open_fresh_wal();
        let (log, done) = r.map_or((-1, -1), |(a, b)| (i64::from(a), i64::from(b)));
        let mut rows = AVec::new();
        push_row(&mut rows, &[int(0), int(log), int(done)]);
        Ok(rows)
    }
}
