//! Schema catalog (SPEC §5.5): `sqlite_schema` scan → catalog in
//! the catalog arena, built by re-parsing each row's stored SQL
//! with hipp's own parser.
//!
//! Names/decls/SQL live in the catalog arena as `ABytes` handles;
//! `TableDef`s themselves are small metadata in an `AVec` (the
//! arena.rs collections rule, §7.5.1). The catalog is rebuilt whole
//! on a schema-cookie change — its arena's only reset (§7.5.2).
//!
//! v0.3: the catalog also carries index definitions. Explicit
//! indexes re-parse their stored `CREATE INDEX` SQL; implicit
//! indexes (`sql IS NULL`, SQLite's `sqlite_autoindex_*`) are
//! derived from the owning table's UNIQUE / PRIMARY KEY constraint
//! sets in statement order — the same order SQLite creates them
//! (oracle-verified). An index whose SQL uses a feature outside
//! the v0.3 grammar (expressions, partial indexes) is recorded
//! as *opaque* (v0.16: NOCASE / RTRIM key collations are
//! maintained, no longer opaque): reads of its table work,
//! writes refuse with `Error::Unsupported` rather than silently
//! desynchronising an index hipp cannot maintain.

use crate::arena::{ABytes, AVec, Arena};
use crate::btree::ScanCursor;
use crate::error::{Error, Result};
use crate::pager::Pager;
use crate::record::{Affinity, Coll, Value, affinity_of, decode_record};
#[cfg(feature = "indexes")]
use crate::sql::ConstraintSet;
use crate::sql::{Ast, Literal, eq_ci, handle_bytes, parse};
use crate::storage::Storage;

#[derive(Debug, Clone)]
pub struct Column {
    pub name: ABytes,
    pub decl: ABytes,
    pub affinity: Affinity,
    pub default: Option<Literal>,
    /// `NOT NULL` appeared in the column's constraints (v0.10:
    /// carried into the catalog so the write paths can enforce
    /// it; parsed since v0.1, recorded for ALTER since v0.4).
    pub not_null: bool,
    /// An expression DEFAULT that could not be folded to a
    /// constant when the catalog was built (v0.16): it names the
    /// clock (`CURRENT_TIMESTAMP`, ...) or fails to evaluate (an
    /// unknown function). The text is evaluated per statement by
    /// the INSERT that needs it. A foldable expression default
    /// (`(1+2)`, `-'3'`) is folded into `default` instead, so
    /// every reader sees the pin's value.
    #[cfg(feature = "query")]
    pub default_expr: Option<ABytes>,
    /// A generated column's expression text (v0.16).
    #[cfg(feature = "query")]
    pub gen_expr: Option<ABytes>,
    /// A STORED generated column (v0.16); VIRTUAL otherwise.
    #[cfg(feature = "query")]
    pub stored: bool,
    /// The column's collating sequence (v0.16; BINARY when no
    /// `COLLATE` clause was written).
    #[cfg(any(feature = "query", feature = "indexes"))]
    pub coll: Coll,
    /// The NOT NULL constraint's own `ON CONFLICT` clause (v0.16,
    /// `sql`; `None`: none).
    #[cfg(feature = "sql")]
    pub nn_conflict: Option<crate::sql::ConflictAction>,
}

/// One column of an index key (v0.3). `col == None` is the rowid
/// (also the canonical form for the table's rowid-alias column).
#[derive(Debug, Clone, Copy)]
pub struct IndexColumn {
    pub col: Option<usize>,
    pub desc: bool,
    /// The key column's collating sequence (v0.16): the index's
    /// explicit `COLLATE`, else the table column's (BINARY for
    /// the rowid).
    pub coll: Coll,
}

/// One UNIQUE / PRIMARY KEY constraint set of a table, resolved
/// to column indices (v0.3): the source of implicit indexes.
#[derive(Debug, Clone)]
pub struct AutoSet {
    pub primary_key: bool,
    pub columns: AVec<IndexColumn>,
    /// The constraint's own `ON CONFLICT` clause (v0.16, `sql`).
    #[cfg(feature = "sql")]
    pub conflict: Option<crate::sql::ConflictAction>,
}

#[derive(Debug, Clone)]
pub struct TableDef {
    pub name: ABytes,
    pub root: u32,
    pub sql: ABytes,
    pub columns: AVec<Column>,
    /// Index of the `INTEGER PRIMARY KEY` column (the rowid alias),
    /// when the table has exactly one.
    pub rowid_alias: Option<usize>,
    /// UNIQUE / PK constraint sets in statement order (v0.3).
    pub auto_sets: AVec<AutoSet>,
    /// CHECK constraint texts, statement order (v0.16).
    #[cfg(feature = "query")]
    pub checks: AVec<ABytes>,
    /// A STRICT table (v0.16, `sql`).
    #[cfg(feature = "sql")]
    pub strict: bool,
    /// The stored SQL names a collating sequence hipp does not
    /// have (an application-registered one, v0.16). The table
    /// is listed (its name is taken, `.schema` shows it) but
    /// has no columns here, and every statement that reads or
    /// writes it is refused with `SchemaKind::UnknownCollation`
    /// (see `TableDef::usable`); the rest of the file works.
    #[cfg(any(feature = "query", feature = "indexes"))]
    pub foreign_coll: bool,
    /// A shadow table of a virtual table whose module hipp lacks,
    /// with stored SQL hipp cannot parse (v0.16, `vtab`: fts3/4
    /// quote their names as strings). Listed, never read or written
    /// (`SchemaKind::NoSuchModule`).
    #[cfg(feature = "vtab")]
    pub shadow_opaque: bool,
    /// The table's INTEGER PRIMARY KEY is AUTOINCREMENT (v0.16,
    /// `sql`): every INSERT keeps its `sqlite_sequence` row.
    #[cfg(feature = "sql")]
    pub autoinc: bool,
    /// The file has a trigger on this table, which a build without
    /// `triggers` cannot run (v0.16): writes are refused
    /// (`SchemaKind::TableHasTriggers`).
    #[cfg(not(feature = "triggers"))]
    pub triggered: bool,
    /// A WITHOUT ROWID table (v0.16, `without_rowid`): `root` is
    /// its clustered index B-tree, its PRIMARY KEY is the automatic
    /// index whose root is `root` (`IndexDef::clustered`), it has
    /// no rowid and no rowid alias.
    #[cfg(feature = "without_rowid")]
    pub without_rowid: bool,
    /// The INTEGER PRIMARY KEY's own `ON CONFLICT` clause (v0.16,
    /// `sql`): how a rowid conflict resolves (the pin's
    /// `keyConf`); `None` when it has none.
    #[cfg(feature = "sql")]
    pub ipk_conflict: Option<crate::sql::ConflictAction>,
}

impl TableDef {
    /// Whether the table has a rowid (every table but a WITHOUT
    /// ROWID one, v0.16): `rowid`, `oid` and `_rowid_` name it.
    pub fn has_rowid(&self) -> bool {
        #[cfg(feature = "without_rowid")]
        if self.without_rowid {
            return false;
        }
        true
    }

    /// The table, unless it names a collating sequence hipp
    /// does not have (v0.16): statements that read or write
    /// such a table are refused with
    /// `SchemaKind::UnknownCollation` — remedy: register no
    /// custom collation, or use an engine that has it.
    pub fn usable<E>(&self) -> Result<&TableDef, E> {
        #[cfg(any(feature = "query", feature = "indexes"))]
        if self.foreign_coll {
            #[cfg(feature = "vtab")]
            if self.shadow_opaque {
                return Err(Error::Schema {
                    kind: crate::error::SchemaKind::NoSuchModule,
                });
            }
            return Err(Error::Schema {
                kind: crate::error::SchemaKind::UnknownCollation,
            });
        }
        Ok(self)
    }

    /// Whether any column is generated (v0.16).
    #[cfg(feature = "query")]
    pub fn has_generated(&self) -> bool {
        self.columns.iter().any(|c| c.gen_expr.is_some())
    }

    /// Whether writes must go through table rules (v0.16): a
    /// generated column, a CHECK constraint, or STRICT typing.
    #[cfg(feature = "query")]
    pub fn has_rules(&self) -> bool {
        #[cfg(feature = "sql")]
        if self.strict {
            return true;
        }
        self.has_generated() || !self.checks.is_empty()
    }
}

/// Where an index came from (v0.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOrigin {
    /// `CREATE INDEX` — has stored SQL, may be dropped.
    Explicit,
    /// A UNIQUE / PRIMARY KEY constraint's implicit index
    /// (`sqlite_autoindex_*`, SQL is NULL) — may not be dropped.
    Auto,
}

#[derive(Debug, Clone)]
pub struct IndexDef {
    pub name: ABytes,
    /// Owning table's name (the schema row's `tbl_name`).
    pub table: ABytes,
    pub root: u32,
    /// Stored CREATE INDEX text; None for implicit indexes.
    pub sql: Option<ABytes>,
    pub unique: bool,
    pub origin: IndexOrigin,
    /// Key columns in order (the rowid is appended by the B-tree
    /// layer, not listed here).
    pub columns: AVec<IndexColumn>,
    /// True when the index's definition uses features outside
    /// v0.3 (see the module docs): it is listed but not
    /// maintainable, and writes to its table are refused.
    pub opaque: bool,
    /// An expression or partial index's extra definition (v0.16,
    /// `expr_index`); `None` for an index on plain columns. Code
    /// that only understands column keys must treat such an index
    /// as it treats an opaque one (see `IndexDef::plain`).
    #[cfg(feature = "expr_index")]
    pub ext: Option<alloc::boxed::Box<IndexExt>>,
    /// The PRIMARY KEY of a WITHOUT ROWID table (v0.16,
    /// `without_rowid`): no schema row, `root` is the table's
    /// clustered tree, and its key columns are the table's key.
    #[cfg(feature = "without_rowid")]
    pub clustered: bool,
    /// A UNIQUE / PRIMARY KEY constraint's own `ON CONFLICT`
    /// clause (v0.16, `sql`): the automatic index's; `None` for
    /// an explicit index and a constraint without one.
    #[cfg(feature = "sql")]
    pub conflict: Option<crate::sql::ConflictAction>,
}

/// What an expression or partial index adds to its key columns
/// (v0.16, `expr_index`): texts re-parsed and resolved against the
/// table whenever the index is maintained or planned.
#[cfg(feature = "expr_index")]
#[derive(Debug, Clone)]
pub struct IndexExt {
    /// Per key column, parallel to `IndexDef::columns`: the item's
    /// expression text for an expression column (its
    /// `IndexColumn::col` is then `None` and its collation the
    /// explicit `COLLATE`, else BINARY, as on the pin).
    pub exprs: AVec<Option<ABytes>>,
    /// The partial index's `WHERE` text.
    pub filter: Option<ABytes>,
}

impl IndexDef {
    /// An index on plain columns hipp maintains (not opaque, and
    /// neither an expression nor a partial index).
    pub fn plain(&self) -> bool {
        #[cfg(feature = "expr_index")]
        if self.ext.is_some() {
            return false;
        }
        !self.opaque
    }

    /// A partial index (v0.16): its stored text has a WHERE.
    #[cfg(feature = "expr_index")]
    pub fn partial(&self) -> bool {
        self.ext.as_ref().is_some_and(|x| x.filter.is_some())
    }
}

/// One view (v0.4): stored SQL, inert (never executed). Column
/// names are derived at catalog build from the view's SELECT —
/// explicit list when written, else item names (SQLite's rule);
/// empty when the stored SQL is outside hipp's grammar (a foreign
/// view: listed, never resolved).
#[derive(Debug, Clone)]
pub struct ViewDef {
    pub name: ABytes,
    pub sql: ABytes,
    pub columns: AVec<ABytes>,
}

/// One trigger (v0.16, `triggers`): its `sqlite_schema` row and
/// what its stored text says. The WHEN clause and the body
/// statements are spans of `sql`, parsed again each time the
/// trigger fires. A text hipp cannot parse makes the trigger
/// `opaque`: writes to its table are refused
/// (`SchemaKind::TableHasTriggers`), never run without it.
#[cfg(feature = "triggers")]
#[derive(Debug, Clone)]
pub struct TriggerDef {
    pub name: ABytes,
    /// The schema row's `tbl_name`: the table (or view) as written
    /// after `ON`.
    pub table: ABytes,
    pub sql: ABytes,
    pub opaque: bool,
    pub time: crate::sql::ast::TrigTime,
    pub event: crate::sql::ast::TrigEvent,
    /// `UPDATE OF` columns; empty for any UPDATE.
    pub of_cols: AVec<ABytes>,
    pub when: Option<(usize, usize)>,
    pub steps: AVec<(usize, usize)>,
}

/// A virtual table of the file (v0.16, `vtab`): its `sqlite_schema`
/// row (type `table`, root page 0, `CREATE VIRTUAL TABLE` text), the
/// module named there and its arguments, and the module's declared
/// table (`Err`: the module is one hipp lacks — the pin's "no such
/// module" — or its arguments are refused; only statements touching
/// the table see that).
#[cfg(feature = "vtab")]
#[derive(Debug)]
pub struct VtabDef {
    pub name: ABytes,
    pub sql: ABytes,
    pub module: ABytes,
    pub args: AVec<ABytes>,
    pub tab: core::result::Result<crate::vtab::VTab, crate::error::SchemaKind>,
}

pub struct Catalog {
    pub tables: AVec<TableDef>,
    /// Virtual tables (v0.16, `vtab`), in `sqlite_schema` order.
    #[cfg(feature = "vtab")]
    pub vtabs: AVec<VtabDef>,
    pub indexes: AVec<IndexDef>,
    pub views: AVec<ViewDef>,
    /// The triggers in `sqlite_schema` rowid (creation) order
    /// (v0.16, `triggers`).
    #[cfg(feature = "triggers")]
    pub triggers: AVec<TriggerDef>,

    /// Schema cookie this catalog was built from.
    pub cookie: u32,
    /// Table and view names in `sqlite_schema` rowid (creation)
    /// order (v0.16, `sql`): the order PRAGMA table_list and
    /// integrity_check walk.
    #[cfg(feature = "sql")]
    pub objects: AVec<ABytes>,
    /// `sqlite_schema` itself as a readable table (v0.16, `sql`):
    /// root page 1, columns `type name tbl_name rootpage sql`.
    #[cfg(feature = "sql")]
    pub schema_table: Option<TableDef>,
}

impl Catalog {
    pub fn empty(cookie: u32) -> Catalog {
        Catalog {
            tables: AVec::new(),
            #[cfg(feature = "vtab")]
            vtabs: AVec::new(),
            indexes: AVec::new(),
            views: AVec::new(),
            #[cfg(feature = "triggers")]
            triggers: AVec::new(),

            cookie,
            #[cfg(feature = "sql")]
            objects: AVec::new(),
            #[cfg(feature = "sql")]
            schema_table: None,
        }
    }

    /// The name names `sqlite_schema` (v0.16, `sql`): any case
    /// of `sqlite_schema` / `sqlite_master`.
    #[cfg(feature = "sql")]
    pub fn is_schema_name(name: &[u8]) -> bool {
        eq_ci(name, b"sqlite_schema") || eq_ci(name, b"sqlite_master")
    }

    /// A table to read (v0.16, `sql`): a catalog table, or
    /// `sqlite_schema` under either of its names.
    #[cfg(feature = "sql")]
    pub fn find_readable<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x TableDef> {
        self.find(arena, name).or_else(|| {
            if Catalog::is_schema_name(name) {
                self.schema_table.as_ref()
            } else {
                None
            }
        })
    }

    /// Whether writes to `table` must be refused for its triggers
    /// (v0.16, every profile): without `triggers`, any trigger on
    /// it; with it, one whose text hipp cannot parse.
    /// `SchemaKind::TableHasTriggers`.
    pub fn trigger_blocked(&self, arena: &Arena, table: &[u8]) -> bool {
        #[cfg(not(feature = "triggers"))]
        return self.find(arena, table).is_some_and(|t| t.triggered);
        #[cfg(feature = "triggers")]
        self.triggers
            .iter()
            .any(|t| t.opaque && eq_ci(handle_bytes(arena, t.table), table))
    }

    /// The triggers on `table` (v0.16, `triggers`), in creation
    /// order.
    #[cfg(feature = "triggers")]
    pub fn triggers_on<'x>(
        &'x self,
        arena: &'x Arena,
        table: &'x [u8],
    ) -> impl Iterator<Item = &'x TriggerDef> + 'x {
        self.triggers
            .iter()
            .filter(move |t| !t.opaque && eq_ci(handle_bytes(arena, t.table), table))
    }

    /// The trigger named `name` (v0.16, `triggers`).
    #[cfg(feature = "triggers")]
    pub fn find_trigger<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x TriggerDef> {
        self.triggers
            .iter()
            .find(|t| eq_ci(handle_bytes(arena, t.name), name))
    }

    pub fn find<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x TableDef> {
        self.tables
            .iter()
            .find(|t| eq_ci(handle_bytes(arena, t.name), name))
    }

    pub fn find_view<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x ViewDef> {
        self.views
            .iter()
            .find(|t| eq_ci(handle_bytes(arena, t.name), name))
    }

    pub fn find_index<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x IndexDef> {
        self.indexes
            .iter()
            .find(|t| eq_ci(handle_bytes(arena, t.name), name))
    }

    /// What a schema name is taken by (v0.16): `Some(true)` for an
    /// index, `Some(false)` for a table or view, `None` when free —
    /// tables, views and indexes share one namespace.
    pub fn taken(&self, arena: &Arena, name: &[u8]) -> Option<bool> {
        let hit = |h: ABytes| eq_ci(handle_bytes(arena, h), name);
        if self.indexes.iter().any(|i| hit(i.name)) {
            Some(true)
        } else if self.tables.iter().any(|t| hit(t.name)) || self.views.iter().any(|v| hit(v.name))
        {
            Some(false)
        } else {
            #[cfg(feature = "vtab")]
            if self.vtabs.iter().any(|v| hit(v.name)) {
                return Some(false);
            }
            None
        }
    }

    /// The virtual table of this name (v0.16, `vtab`).
    #[cfg(feature = "vtab")]
    pub fn find_vtab<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x VtabDef> {
        self.vtabs
            .iter()
            .find(|v| eq_ci(handle_bytes(arena, v.name), name))
    }

    /// Whether `name` is a shadow table of one of the file's virtual
    /// tables (v0.16, `vtab`; the pin's `sqlite3ShadowTableName`:
    /// `<vtab>_<suffix>` where the vtab's module claims the suffix,
    /// or any suffix when hipp lacks the module).
    #[cfg(feature = "vtab")]
    pub fn is_shadow(&self, arena: &Arena, name: &[u8]) -> bool {
        self.shadow_of(arena, name, true)
    }

    /// Whether `name` is reserved for a shadow table of one of the
    /// file's virtual tables whose module hipp has (v0.16 review fix,
    /// `vtab`): the pin's "object name reserved for internal use" for
    /// CREATE TABLE / VIEW / INDEX / TRIGGER and RENAME TO, which an
    /// unknown module's table does not reserve.
    #[cfg(feature = "vtab")]
    pub fn shadow_reserved(&self, arena: &Arena, name: &[u8]) -> bool {
        self.shadow_of(arena, name, false)
    }

    #[cfg(feature = "vtab")]
    fn shadow_of(&self, arena: &Arena, name: &[u8], unknown: bool) -> bool {
        use crate::vtab::Vtab;
        self.vtabs.iter().any(|v| {
            let vn = handle_bytes(arena, v.name);
            name.len() > vn.len() + 1
                && name.get(..vn.len()).is_some_and(|p| eq_ci(p, vn))
                && name.get(vn.len()) == Some(&b'_')
                // A module hipp lacks may claim any suffix: such a
                // table is treated as its shadow (never written).
                && v.tab.as_ref().map_or(unknown, |t| {
                    t.module
                        .shadow_name(name.get(vn.len() + 1..).unwrap_or(&[]))
                })
        })
    }

    /// All indexes (explicit and implicit) on one table.
    pub fn indexes_for<'x>(
        &'x self,
        arena: &'x Arena,
        table: &'x [u8],
    ) -> impl Iterator<Item = &'x IndexDef> + 'x {
        self.indexes
            .iter()
            .filter(move |i| eq_ci(handle_bytes(arena, i.table), table))
    }
}

/// Copy bytes from one arena (or any source) into the catalog arena.
fn intern(arena: &Arena, bytes: &[u8]) -> core::result::Result<ABytes, crate::error::ArenaError> {
    arena.alloc_copy_handle(bytes)
}

fn intern_literal(
    arena: &Arena,
    lit: Literal,
    src_arena: &Arena,
) -> core::result::Result<Literal, crate::error::ArenaError> {
    Ok(match lit {
        Literal::Text(h) => Literal::Text(intern(arena, handle_bytes(src_arena, h))?),
        Literal::Blob(h) => Literal::Blob(intern(arena, handle_bytes(src_arena, h))?),
        other => other,
    })
}

/// A folded DEFAULT value as a catalog literal (v0.16).
#[cfg(feature = "query")]
fn owned_literal(
    arena: &Arena,
    v: &crate::record::OwnedValue,
) -> core::result::Result<Literal, crate::error::ArenaError> {
    use crate::record::OwnedValue;
    Ok(match v {
        OwnedValue::Null => Literal::Null,
        OwnedValue::Integer(i) => Literal::Integer(*i),
        OwnedValue::Real(f) => Literal::Real(*f),
        OwnedValue::Text(t) => Literal::Text(intern(arena, t.as_slice())?),
        OwnedValue::Blob(b) => Literal::Blob(intern(arena, b.as_slice())?),
    })
}

/// One raw `sqlite_schema` row, in scan (creation) order.
struct SchemaRow {
    ty: AVec<u8>,
    name: AVec<u8>,
    tbl_name: AVec<u8>,
    root: u32,
    sql: Option<AVec<u8>>,
}

fn copy_avec(b: &[u8]) -> AVec<u8> {
    let mut a = AVec::new();
    a.extend_from_slice(b);
    a
}

/// Resolve an index column name against a table: a real column
/// index, or None for the rowid pseudo-column (including the
/// rowid-alias column, canonicalised). Err(()) = unknown column.
#[cfg(feature = "indexes")]
fn resolve_index_column(
    arena: &Arena,
    t: &TableDef,
    name: &[u8],
) -> core::result::Result<Option<usize>, ()> {
    for (i, c) in t.columns.iter().enumerate() {
        if eq_ci(handle_bytes(arena, c.name), name) {
            if Some(i) == t.rowid_alias {
                return Ok(None);
            }
            return Ok(Some(i));
        }
    }
    if eq_ci(name, b"rowid") || eq_ci(name, b"oid") || eq_ci(name, b"_rowid_") {
        return Ok(None);
    }
    Err(())
}

/// The collating sequence of index key column `col` (v0.16):
/// the table column's, BINARY for the rowid.
#[cfg(feature = "indexes")]
fn col_coll(t: &TableDef, col: Option<usize>) -> Coll {
    col.and_then(|i| t.columns.as_slice().get(i))
        .map(|c| c.coll)
        .unwrap_or_default()
}

/// List each WITHOUT ROWID table's PRIMARY KEY among its indexes
/// (v0.16, `without_rowid`): an automatic index on the table's own
/// root, numbered by its constraint's place among the table's
/// UNIQUE / PRIMARY KEY sets, placed where its schema row would be
/// (after the automatic indexes of earlier sets, before later ones
/// and before every explicit index).
#[cfg(feature = "without_rowid")]
fn place_clustered_keys(
    catalog: &mut Catalog,
    arena: &Arena,
) -> core::result::Result<(), crate::error::ArenaError> {
    let mut adds: AVec<(usize, IndexDef)> = AVec::new();
    for t in catalog.tables.iter().filter(|t| t.without_rowid) {
        let Some(k) = t.auto_sets.iter().position(|s| s.primary_key) else {
            continue;
        };
        let tname = handle_bytes(arena, t.name);
        let mut name = AVec::new();
        name.extend_from_slice(b"sqlite_autoindex_");
        name.extend_from_slice(tname);
        name.push(b'_');
        crate::record::render_int(k as i64 + 1, &mut name);
        let mut at = None;
        let mut autos = 0usize;
        let mut last = None;
        for (i, ix) in catalog.indexes.iter().enumerate() {
            if !eq_ci(handle_bytes(arena, ix.table), tname) {
                continue;
            }
            last = Some(i);
            if ix.origin == IndexOrigin::Explicit || autos == k {
                at = Some(i);
                break;
            }
            autos += 1;
        }
        let at = at.unwrap_or_else(|| last.map_or(catalog.indexes.len(), |l| l + 1));
        let Some(set) = t.auto_sets.as_slice().get(k) else {
            continue;
        };
        let columns = set.columns.clone();
        #[cfg(feature = "sql")]
        let conflict = set.conflict;
        adds.push((
            at,
            IndexDef {
                name: intern(arena, name.as_slice())?,
                table: t.name,
                root: t.root,
                sql: None,
                unique: true,
                origin: IndexOrigin::Auto,
                columns,
                opaque: false,
                #[cfg(feature = "expr_index")]
                ext: None,
                clustered: true,
                #[cfg(feature = "sql")]
                conflict,
            },
        ));
    }
    // Later positions first, so earlier ones stay valid.
    crate::arena::sort_by(adds.as_mut_slice(), &mut |a, b| a.0.cmp(&b.0));
    while let Some((at, ix)) = adds.pop() {
        catalog.indexes.insert_at(at.min(catalog.indexes.len()), ix);
    }
    Ok(())
}

/// Build the catalog by scanning `sqlite_schema` (root page 1).
///
/// The future only reads the schema rows; decoding a row and
/// building the catalog from the collected rows are sync
/// ([`collect_schema_row`], [`catalog_from_rows`]), so that code is
/// not carried inside the state machine (ADR 0005 Stage 1).
pub async fn build_catalog<S: Storage>(
    pager: &mut Pager<S>,
    catalog_arena: &Arena,
    lim: crate::sql::parser::Limits,
) -> Result<Catalog, S::Error> {
    // Pass 0: collect the raw schema rows (tables must be built
    // before indexes can resolve their columns).
    let mut rows: AVec<SchemaRow> = AVec::new();
    {
        let mut cursor = ScanCursor::new(1);
        while let Some((_rowid, payload)) = cursor.next_row(pager).await? {
            collect_schema_row::<S::Error>(&mut rows, payload.as_slice())?;
        }
    }
    catalog_from_rows::<S::Error>(&rows, pager.schema_cookie(), catalog_arena, lim)
}

/// Decode one `sqlite_schema` record into `rows`; a record without
/// the five text/integer columns is skipped.
#[inline(never)]
fn collect_schema_row<E>(rows: &mut AVec<SchemaRow>, payload: &[u8]) -> Result<(), E> {
    let values = decode_record::<E>(payload, 5)?;
    if values.len() < 5 {
        return Ok(());
    }
    let (Value::Text(ty), Value::Text(name), Value::Text(tbl_name), Value::Integer(root)) = (
        &values.as_slice()[0],
        &values.as_slice()[1],
        &values.as_slice()[2],
        &values.as_slice()[3],
    ) else {
        return Ok(());
    };
    let sql = match &values.as_slice()[4] {
        Value::Text(s) => Some(copy_avec(s)),
        _ => None,
    };
    rows.push(SchemaRow {
        ty: copy_avec(ty),
        name: copy_avec(name),
        tbl_name: copy_avec(tbl_name),
        root: *root as u32,
        sql,
    });
    Ok(())
}

/// Build the catalog from the collected schema rows (sync: no I/O).
#[inline(never)]
fn catalog_from_rows<E>(
    rows: &AVec<SchemaRow>,
    cookie: u32,
    catalog_arena: &Arena,
    lim: crate::sql::parser::Limits,
) -> Result<Catalog, E> {
    let mut catalog = Catalog::empty(cookie);
    #[cfg(feature = "sql")]
    {
        for row in rows.iter() {
            if eq_ci(row.ty.as_slice(), b"table") || eq_ci(row.ty.as_slice(), b"view") {
                catalog
                    .objects
                    .push(intern(catalog_arena, row.name.as_slice())?);
            }
        }
        let mut columns = AVec::new();
        for (n, d) in [
            (b"type" as &[u8], b"TEXT" as &[u8]),
            (b"name", b"TEXT"),
            (b"tbl_name", b"TEXT"),
            (b"rootpage", b"INT"),
            (b"sql", b"TEXT"),
        ] {
            columns.push(Column {
                name: intern(catalog_arena, n)?,
                decl: intern(catalog_arena, d)?,
                affinity: affinity_of(d),
                default: None,
                not_null: false,
                default_expr: None,
                gen_expr: None,
                stored: false,
                coll: Coll::Binary,
                nn_conflict: None,
            });
        }
        catalog.schema_table = Some(TableDef {
            name: intern(catalog_arena, b"sqlite_master")?,
            root: 1,
            sql: ABytes::EMPTY,
            columns,
            rowid_alias: None,
            auto_sets: AVec::new(),
            checks: AVec::new(),
            strict: false,
            foreign_coll: false,
            #[cfg(feature = "vtab")]
            shadow_opaque: false,
            autoinc: false,
            #[cfg(not(feature = "triggers"))]
            triggered: false,
            #[cfg(feature = "without_rowid")]
            without_rowid: false,
            ipk_conflict: None,
        });
    }
    // Scratch arena for parse trees (names are interned out of it
    // into the catalog arena before the next row).
    let scratch = Arena::new(crate::error::Scope::Statement, 0, 1 << 20);

    // Pass 1: tables (+ their constraint sets).
    for row in rows.iter() {
        if !eq_ci(row.ty.as_slice(), b"table") || row.sql.is_none() {
            continue; // indexes in pass 2; views in pass 3
        }
        let sql = row.sql.as_ref().expect("checked");
        scratch.reset();
        // A virtual table (v0.16, `vtab`): listed with its module's
        // declared columns (none when hipp lacks the module).
        #[cfg(feature = "vtab")]
        if let Ok(Ast::CreateVirtual { module, args, .. }) =
            &parse::<E>(sql.as_slice(), &scratch, lim)
        {
            let m = handle_bytes(&scratch, *module);
            let mut spans: AVec<&[u8]> = AVec::new();
            let mut stored = AVec::new();
            for a in args.iter() {
                let t = handle_bytes(&scratch, *a);
                spans.push(t);
                stored.push(intern(catalog_arena, t)?);
            }
            let tab = match crate::vtab::Module::lookup(m) {
                Some(md) if crate::vtab::Vtab::creatable(&md) => {
                    crate::vtab::Vtab::connect(&md, row.name.as_slice(), spans.as_slice(), false)
                }
                _ => Err(crate::error::SchemaKind::NoSuchModule),
            };
            catalog.vtabs.push(VtabDef {
                name: intern(catalog_arena, row.name.as_slice())?,
                sql: intern(catalog_arena, sql.as_slice())?,
                module: intern(catalog_arena, m)?,
                args: stored,
                tab,
            });
            continue;
        }
        let ast = match parse::<E>(sql.as_slice(), &scratch, lim) {
            Ok(a) => a,
            // v0.16: a collation hipp does not have (one an
            // application registered) — the pin opens such a
            // file and errors only where the collation is used.
            // Keep the table listed but unusable; the rest of
            // the file still opens.
            #[cfg(any(feature = "query", feature = "indexes"))]
            Err(e)
                if matches!(
                    e,
                    Error::Schema {
                        kind: crate::error::SchemaKind::UnknownCollation,
                    }
                ) || {
                    // A shadow table of a module hipp lacks (v0.16,
                    // `vtab`), whatever its text.
                    #[cfg(feature = "vtab")]
                    {
                        catalog.is_shadow(catalog_arena, row.name.as_slice())
                    }
                    #[cfg(not(feature = "vtab"))]
                    {
                        false
                    }
                } =>
            {
                #[cfg(feature = "vtab")]
                let shadow_opaque = !matches!(
                    e,
                    Error::Schema {
                        kind: crate::error::SchemaKind::UnknownCollation,
                    }
                );
                #[cfg(not(feature = "vtab"))]
                let _ = e;
                catalog.tables.push(TableDef {
                    name: intern(catalog_arena, row.name.as_slice())?,
                    root: row.root,
                    sql: intern(catalog_arena, sql.as_slice())?,
                    columns: AVec::new(),
                    rowid_alias: None,
                    auto_sets: AVec::new(),
                    #[cfg(feature = "query")]
                    checks: AVec::new(),
                    #[cfg(feature = "sql")]
                    strict: false,
                    foreign_coll: true,
                    #[cfg(feature = "vtab")]
                    shadow_opaque,
                    #[cfg(feature = "sql")]
                    autoinc: false,
                    #[cfg(not(feature = "triggers"))]
                    triggered: false,
                    #[cfg(feature = "without_rowid")]
                    without_rowid: false,
                    #[cfg(feature = "sql")]
                    ipk_conflict: None,
                });
                continue;
            }
            Err(e) => return Err(e),
        };
        let Ast::CreateTable {
            columns,
            pk_columns,
            constraint_sets,
            #[cfg(feature = "query")]
            checks,
            #[cfg(feature = "sql")]
            strict,
            #[cfg(feature = "without_rowid")]
            without_rowid,
            ..
        } = ast
        else {
            return Err(Error::CorruptRecord);
        };
        #[cfg(not(feature = "without_rowid"))]
        let without_rowid = false;
        let mut cols: AVec<Column> = AVec::new();
        let mut alias = None;
        for (i, c) in columns.iter().enumerate() {
            let decl_bytes = handle_bytes(&scratch, c.decl);
            // Rowid alias rule: exactly one PK column (below), of
            // declared type exactly INTEGER, not written DESC.
            if pk_columns.len() == 1
                && !without_rowid
                && eq_ci(
                    handle_bytes(&scratch, c.name),
                    handle_bytes(&scratch, pk_columns.as_slice()[0]),
                )
                && !c.pk_desc
                && eq_ci(crate::record::decl_token(decl_bytes), b"INTEGER")
            {
                alias = Some(i);
            }
            #[allow(unused_mut)]
            let mut affinity = affinity_of(decl_bytes);
            // STRICT (v0.16): an ANY column has no affinity —
            // values are stored as given (in an ordinary table
            // `ANY` is NUMERIC by the name rules).
            #[cfg(feature = "sql")]
            if strict && eq_ci(crate::record::decl_token(decl_bytes), b"ANY") {
                affinity = Affinity::Blob;
            }
            #[allow(unused_mut)]
            let mut default = match c.default {
                Some(l) => Some(intern_literal(catalog_arena, l, &scratch)?),
                None => None,
            };
            // v0.16: fold an expression DEFAULT once, here, when
            // it is constant; keep its text when it reads the
            // clock or fails to evaluate (the INSERT that needs
            // it evaluates it then — and errors, as the pin does).
            #[cfg(feature = "query")]
            let mut default_expr = None;
            #[cfg(feature = "query")]
            if let Some(h) = c.default_expr {
                let text = handle_bytes(&scratch, h);
                match crate::rules::eval_default::<E>(
                    text,
                    &scratch,
                    lim,
                    &mut crate::rules::Clock::new(None),
                    None,
                ) {
                    Ok(v) => default = Some(owned_literal(catalog_arena, &v)?),
                    Err(_) => default_expr = Some(intern(catalog_arena, text)?),
                }
            }
            // A WITHOUT ROWID table's key columns are NOT NULL
            // (v0.16; the pin marks them so, and PRAGMA table_info
            // shows it).
            let in_pk = without_rowid
                && pk_columns
                    .iter()
                    .any(|p| eq_ci(handle_bytes(&scratch, *p), handle_bytes(&scratch, c.name)));
            cols.push(Column {
                name: intern(catalog_arena, handle_bytes(&scratch, c.name))?,
                decl: intern(catalog_arena, decl_bytes)?,
                affinity,
                default,
                not_null: c.not_null || in_pk,
                #[cfg(feature = "query")]
                default_expr,
                #[cfg(feature = "query")]
                gen_expr: match c.gen_expr {
                    Some(h) => Some(intern(catalog_arena, handle_bytes(&scratch, h))?),
                    None => None,
                },
                #[cfg(feature = "query")]
                stored: c.stored,
                #[cfg(any(feature = "query", feature = "indexes"))]
                coll: c.coll.unwrap_or_default(),
                #[cfg(feature = "sql")]
                nn_conflict: if c.not_null { c.nn_conflict } else { None },
            });
        }
        let rowid_alias = alias;
        // The rowid alias's PRIMARY KEY clause (v0.16, `sql`).
        #[cfg(feature = "sql")]
        let ipk_conflict = match rowid_alias {
            Some(_) => constraint_sets
                .iter()
                .find(|s| s.primary_key)
                .and_then(|s| s.conflict),
            None => None,
        };
        #[cfg(feature = "indexes")]
        #[allow(unused_mut)]
        let mut auto_sets = resolve_auto_sets(
            &scratch,
            catalog_arena,
            cols.as_slice(),
            constraint_sets.as_slice(),
            &rowid_alias,
            &pk_columns,
        );
        // A WITHOUT ROWID key drops a column repeated with the same
        // collation (v0.16; the pin's `isDupColumn`).
        #[cfg(feature = "without_rowid")]
        if without_rowid {
            for set in auto_sets.as_mut_slice().iter_mut() {
                if !set.primary_key {
                    continue;
                }
                let mut kept: AVec<IndexColumn> = AVec::new();
                for c in set.columns.iter() {
                    if !kept.iter().any(|k| k.col == c.col && k.coll == c.coll) {
                        kept.push(*c);
                    }
                }
                set.columns = kept;
            }
        }
        #[cfg(not(feature = "indexes"))]
        let auto_sets: AVec<AutoSet> = AVec::new();
        #[cfg(not(feature = "indexes"))]
        let _ = (constraint_sets, pk_columns);
        #[cfg(feature = "query")]
        let mut check_texts = AVec::new();
        #[cfg(feature = "query")]
        for h in checks.iter() {
            check_texts.push(intern(catalog_arena, handle_bytes(&scratch, *h))?);
        }
        catalog.tables.push(TableDef {
            name: intern(catalog_arena, row.name.as_slice())?,
            root: row.root,
            sql: intern(catalog_arena, sql.as_slice())?,
            columns: cols,
            rowid_alias,
            auto_sets,
            #[cfg(feature = "query")]
            checks: check_texts,
            #[cfg(feature = "sql")]
            strict,
            #[cfg(any(feature = "query", feature = "indexes"))]
            foreign_coll: false,
            #[cfg(feature = "vtab")]
            shadow_opaque: false,
            #[cfg(feature = "sql")]
            autoinc: rowid_alias.is_some()
                && crate::sql::parser::has_autoincrement::<E>(sql.as_slice(), &scratch),
            #[cfg(not(feature = "triggers"))]
            triggered: rows.iter().any(|r| {
                eq_ci(r.ty.as_slice(), b"trigger")
                    && eq_ci(r.tbl_name.as_slice(), row.name.as_slice())
            }),
            #[cfg(feature = "without_rowid")]
            without_rowid,
            #[cfg(feature = "sql")]
            ipk_conflict,
        });
    }

    // Pass 2: indexes.
    for row in rows.iter() {
        if !eq_ci(row.ty.as_slice(), b"index") {
            continue;
        }
        let Some(t) = catalog
            .tables
            .iter()
            .find(|t| eq_ci(handle_bytes(catalog_arena, t.name), row.tbl_name.as_slice()))
            .cloned()
        else {
            continue; // index on a table hipp did not load
        };
        // Without the `indexes` feature the catalog only records
        // that an index exists (reads still work; the tiny
        // profile never maintains or enforces indexes).
        #[cfg(not(feature = "indexes"))]
        {
            let _ = &t;
            catalog.indexes.push(IndexDef {
                name: intern(catalog_arena, row.name.as_slice())?,
                table: intern(catalog_arena, row.tbl_name.as_slice())?,
                root: row.root,
                sql: match &row.sql {
                    Some(sql) => Some(intern(catalog_arena, sql.as_slice())?),
                    None => None,
                },
                unique: false,
                origin: if row.sql.is_some() {
                    IndexOrigin::Explicit
                } else {
                    IndexOrigin::Auto
                },
                columns: AVec::new(),
                opaque: true,
                #[cfg(feature = "without_rowid")]
                clustered: false,
                #[cfg(feature = "sql")]
                conflict: None,
            });
            continue;
        }
        #[cfg(feature = "indexes")]
        match &row.sql {
            Some(sql) => {
                // Explicit index: re-parse its CREATE INDEX.
                scratch.reset();
                let parsed = parse::<E>(sql.as_slice(), &scratch, lim);
                let mut idx = IndexDef {
                    name: intern(catalog_arena, row.name.as_slice())?,
                    table: intern(catalog_arena, row.tbl_name.as_slice())?,
                    root: row.root,
                    sql: Some(intern(catalog_arena, sql.as_slice())?),
                    unique: false,
                    origin: IndexOrigin::Explicit,
                    columns: AVec::new(),
                    opaque: false,
                    #[cfg(feature = "expr_index")]
                    ext: None,
                    #[cfg(feature = "without_rowid")]
                    clustered: false,
                    #[cfg(feature = "sql")]
                    conflict: None,
                };
                match parsed {
                    Ok(Ast::CreateIndex {
                        unique,
                        columns,
                        #[cfg(feature = "expr_index")]
                        filter,
                        ..
                    }) => {
                        idx.unique = unique;
                        let mut ok = true;
                        #[cfg(feature = "expr_index")]
                        let mut ext = IndexExt {
                            exprs: AVec::new(),
                            filter: match filter {
                                Some(h) => Some(intern(catalog_arena, handle_bytes(&scratch, h))?),
                                None => None,
                            },
                        };
                        for cd in columns.iter() {
                            #[cfg(feature = "expr_index")]
                            if let Some(h) = cd.expr {
                                ext.exprs
                                    .push(Some(intern(catalog_arena, handle_bytes(&scratch, h))?));
                                idx.columns.push(IndexColumn {
                                    col: None,
                                    desc: cd.desc,
                                    coll: cd.coll.unwrap_or_default(),
                                });
                                continue;
                            }
                            #[cfg(feature = "expr_index")]
                            ext.exprs.push(None);
                            let nb = handle_bytes(&scratch, cd.name);
                            match resolve_index_column(catalog_arena, &t, nb) {
                                Ok(col) => idx.columns.push(IndexColumn {
                                    col,
                                    desc: cd.desc,
                                    coll: cd.coll.unwrap_or_else(|| col_coll(&t, col)),
                                }),
                                Err(()) => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        idx.opaque = !ok;
                        #[cfg(feature = "expr_index")]
                        if ext.filter.is_some() || ext.exprs.iter().any(|e| e.is_some()) {
                            idx.ext = Some(alloc::boxed::Box::new(ext));
                        }
                    }
                    _ => {
                        // Unparseable by the v0.3 grammar
                        // (expressions, partial, collations, ...).
                        idx.opaque = true;
                    }
                }
                catalog.indexes.push(idx);
            }
            None => {
                // Implicit index: consume the next unclaimed
                // constraint set of the owning table, in order.
                let pos = catalog
                    .indexes
                    .iter()
                    .filter(|i| {
                        i.origin == IndexOrigin::Auto
                            && eq_ci(
                                handle_bytes(catalog_arena, i.table),
                                row.tbl_name.as_slice(),
                            )
                    })
                    .count();
                // A WITHOUT ROWID table's PRIMARY KEY has no row of
                // its own (v0.16): the rows are its other sets'.
                #[cfg(feature = "without_rowid")]
                let pos = match t.auto_sets.iter().position(|s| s.primary_key) {
                    Some(k) if t.without_rowid && pos >= k => pos + 1,
                    _ => pos,
                };
                let set = t.auto_sets.as_slice().get(pos).cloned();
                let (unique, columns, opaque) = match &set {
                    Some(s) => (true, s.columns.clone(), false),
                    None => (true, AVec::new(), true),
                };
                catalog.indexes.push(IndexDef {
                    name: intern(catalog_arena, row.name.as_slice())?,
                    table: intern(catalog_arena, row.tbl_name.as_slice())?,
                    root: row.root,
                    sql: None,
                    unique,
                    origin: IndexOrigin::Auto,
                    columns,
                    opaque,
                    #[cfg(feature = "expr_index")]
                    ext: None,
                    #[cfg(feature = "without_rowid")]
                    clustered: false,
                    #[cfg(feature = "sql")]
                    conflict: set.as_ref().and_then(|s| s.conflict),
                });
            }
        }
    }
    // The PRIMARY KEY of each WITHOUT ROWID table (v0.16): listed
    // with the table's automatic indexes, in its constraint's
    // place, named by its number among them (the pin keeps no
    // schema row for it).
    #[cfg(feature = "without_rowid")]
    place_clustered_keys(&mut catalog, catalog_arena)?;
    // Pass 3: views (v0.4) — stored SQL, inert. Column names are
    // derived from the stored SELECT against the tables and the
    // views already derived (views may layer on views).
    for row in rows.iter() {
        if !eq_ci(row.ty.as_slice(), b"view") || row.sql.is_none() {
            continue;
        }
        let sql = row.sql.as_ref().expect("checked");
        let mut columns: AVec<ABytes> = AVec::new();
        scratch.reset();
        if let Ok(Ast::CreateView {
            columns: explicit,
            select,
            ..
        }) = parse::<E>(sql.as_slice(), &scratch, lim)
        {
            columns = derive_view_columns(&catalog, catalog_arena, &scratch, &select, &explicit);
        }
        catalog.views.push(ViewDef {
            name: intern(catalog_arena, row.name.as_slice())?,
            sql: intern(catalog_arena, sql.as_slice())?,
            columns,
        });
    }

    // Pass 4: triggers (v0.16, `triggers`). Without the feature
    // pass 1 notes the tables they are on (`TableDef::triggered`):
    // writes to those are refused (never silently skipping a
    // trigger the file defines).
    #[cfg(feature = "triggers")]
    for row in rows.iter() {
        if !eq_ci(row.ty.as_slice(), b"trigger") {
            continue;
        }
        {
            scratch.reset();
            let sql = row.sql.as_ref().map_or(&[][..], |s| s.as_slice());
            let parsed = match parse::<E>(sql, &scratch, lim) {
                Ok(Ast::CreateTrigger(t)) => Some(t),
                _ => None,
            };
            let mut of_cols = AVec::new();
            if let Some(t) = &parsed {
                for c in t.of_cols.iter() {
                    of_cols.push(intern(catalog_arena, handle_bytes(&scratch, *c))?);
                }
            }
            catalog.triggers.push(TriggerDef {
                name: intern(catalog_arena, row.name.as_slice())?,
                table: intern(catalog_arena, row.tbl_name.as_slice())?,
                sql: intern(catalog_arena, sql)?,
                opaque: parsed.is_none(),
                time: parsed
                    .as_ref()
                    .map_or(crate::sql::ast::TrigTime::Before, |t| t.time),
                event: parsed
                    .as_ref()
                    .map_or(crate::sql::ast::TrigEvent::Insert, |t| t.event),
                of_cols,
                when: parsed.as_ref().and_then(|t| t.when),
                steps: parsed.map(|t| t.steps).unwrap_or_default(),
            });
        }
    }

    catalog.cookie = cookie;
    Ok(catalog)
}

/// Derive a view's column names (SQLite's rule): the explicit
/// column list when one was written, else per SELECT item — a
/// column reference contributes its column name, an alias wins,
/// `*` expands, and any other expression is named by its source
/// text. Any unresolvable piece yields an empty list (the view is
/// still catalogued; only its derived names are unknown).
fn derive_view_columns(
    catalog: &Catalog,
    catalog_arena: &Arena,
    scratch: &Arena,
    select: &crate::sql::SelectStmt,
    explicit: &Option<AVec<ABytes>>,
) -> AVec<ABytes> {
    let mut out: AVec<ABytes> = AVec::new();
    if let Some(cols) = explicit {
        for c in cols.iter() {
            match intern(catalog_arena, handle_bytes(scratch, *c)) {
                Ok(h) => out.push(h),
                Err(_) => return AVec::new(),
            }
        }
        return out;
    }
    // A VALUES view (v0.16): `column1`, `column2`, ... as on the
    // pin.
    #[cfg(feature = "query")]
    if let Some(row) = select.values.as_slice().first() {
        for i in 0..row.len() {
            let mut n = copy_avec(b"column");
            let mut digits = AVec::new();
            let mut k = i + 1;
            while k > 0 {
                digits.push(b'0' + (k % 10) as u8);
                k /= 10;
            }
            while let Some(d) = digits.pop() {
                n.push(d);
            }
            match intern(catalog_arena, n.as_slice()) {
                Ok(h) => out.push(h),
                Err(_) => return AVec::new(),
            }
        }
        return out;
    }
    // Source column-name lists, in FROM order — and (v0.16)
    // each source's NATURAL / USING join columns: an
    // unqualified `*` shows those once, at the left-most source
    // (the pin's `selectExpander`; review regression: they were
    // listed per source).
    let mut sources: AVec<(AVec<u8>, AVec<AVec<u8>>)> = AVec::new();
    #[cfg(feature = "query")]
    let mut joined: AVec<AVec<AVec<u8>>> = AVec::new();
    for src in select.from.iter() {
        let tname = handle_bytes(scratch, src.table);
        let qualifier = match src.alias {
            Some(a) => copy_avec(handle_bytes(scratch, a)),
            None => copy_avec(tname),
        };
        let mut cols: AVec<AVec<u8>> = AVec::new();
        if let Some(t) = catalog.find(catalog_arena, tname) {
            for c in t.columns.iter() {
                cols.push(copy_avec(handle_bytes(catalog_arena, c.name)));
            }
        } else if let Some(v) = catalog.find_view(catalog_arena, tname) {
            if v.columns.is_empty() {
                return AVec::new();
            }
            for c in v.columns.iter() {
                cols.push(copy_avec(handle_bytes(catalog_arena, *c)));
            }
        } else {
            return AVec::new();
        }
        #[cfg(feature = "query")]
        {
            let mut j: AVec<AVec<u8>> = AVec::new();
            if src.natural {
                for c in cols.iter() {
                    if sources
                        .iter()
                        .any(|(_, l)| l.iter().any(|x| eq_ci(x.as_slice(), c.as_slice())))
                    {
                        j.push(c.clone());
                    }
                }
            } else {
                for u in src.using.iter() {
                    j.push(copy_avec(handle_bytes(scratch, *u)));
                }
            }
            joined.push(j);
        }
        sources.push((qualifier, cols));
    }
    for item in select.items.iter() {
        match item {
            crate::sql::SelectItem::Star => {
                #[cfg(feature = "query")]
                let mut joined_it = joined.iter();
                for (_, cols) in sources.iter() {
                    #[cfg(feature = "query")]
                    let j = joined_it.next();
                    for c in cols.iter() {
                        #[cfg(feature = "query")]
                        if j.is_some_and(|j| j.iter().any(|x| eq_ci(x.as_slice(), c.as_slice()))) {
                            continue;
                        }
                        match intern(catalog_arena, c.as_slice()) {
                            Ok(h) => out.push(h),
                            Err(_) => return AVec::new(),
                        }
                    }
                }
            }
            crate::sql::SelectItem::TableStar(q) => {
                let qb = handle_bytes(scratch, *q);
                let mut hit = false;
                for (qual, cols) in sources.iter() {
                    if eq_ci(qual.as_slice(), qb) {
                        hit = true;
                        for c in cols.iter() {
                            match intern(catalog_arena, c.as_slice()) {
                                Ok(h) => out.push(h),
                                Err(_) => return AVec::new(),
                            }
                        }
                    }
                }
                if !hit {
                    return AVec::new();
                }
            }
            crate::sql::SelectItem::Expr(e, alias, text) => {
                let name: &[u8] = if let Some(a) = alias {
                    handle_bytes(scratch, *a)
                } else if let crate::sql::Expr::Column { table, name, .. } = e {
                    // v0.16: the referenced column's own spelling
                    // (the pin names `SELECT A FROM s` column `a`).
                    let nb = handle_bytes(scratch, *name);
                    let qb = table.map(|q| handle_bytes(scratch, q));
                    let hit = sources
                        .iter()
                        .filter(|(q, _)| qb.is_none_or(|qb| eq_ci(q.as_slice(), qb)))
                        .flat_map(|(_, cols)| cols.iter())
                        .find(|c| eq_ci(c.as_slice(), nb));
                    match hit {
                        Some(c) => c.as_slice(),
                        // v0.16: a rowid reference is named by the
                        // rowid-alias column, as on the pin.
                        #[cfg(feature = "query")]
                        None if eq_ci(nb, b"rowid")
                            || eq_ci(nb, b"oid")
                            || eq_ci(nb, b"_rowid_") =>
                        {
                            let alias = select
                                .from
                                .iter()
                                .zip(sources.iter())
                                .filter(|(_, (q, _))| qb.is_none_or(|qb| eq_ci(q.as_slice(), qb)))
                                .find_map(|(src, _)| {
                                    let t = catalog
                                        .find(catalog_arena, handle_bytes(scratch, src.table))?;
                                    let a = t.rowid_alias?;
                                    t.columns
                                        .as_slice()
                                        .get(a)
                                        .map(|c| handle_bytes(catalog_arena, c.name))
                                });
                            alias.unwrap_or(b"rowid")
                        }
                        None => nb,
                    }
                } else {
                    handle_bytes(scratch, *text)
                };
                match intern(catalog_arena, name) {
                    Ok(h) => out.push(h),
                    Err(_) => return AVec::new(),
                }
            }
        }
    }
    // Duplicate names become `id:1`, ... as on the pin (v0.16).
    #[cfg(feature = "query")]
    {
        let mut names: AVec<AVec<u8>> = AVec::new();
        for h in out.iter() {
            names.push(copy_avec(handle_bytes(catalog_arena, *h)));
        }
        if crate::rules::unique_names::<()>(names.as_mut_slice()).is_err() {
            return AVec::new();
        }
        for (h, n) in out.as_mut_slice().iter_mut().zip(names.iter()) {
            if handle_bytes(catalog_arena, *h) != n.as_slice() {
                match intern(catalog_arena, n.as_slice()) {
                    Ok(x) => *h = x,
                    Err(_) => return AVec::new(),
                }
            }
        }
    }
    out
}

/// Constraint sets resolved to column indices (v0.3 index
/// metadata). A set whose column does not resolve (corrupt
/// foreign SQL) is kept out — its implicit index, if any,
/// becomes opaque in pass 2.
#[cfg(feature = "indexes")]
fn resolve_auto_sets(
    scratch: &Arena,
    catalog_arena: &Arena,
    cols: &[Column],
    constraint_sets: &[ConstraintSet],
    rowid_alias: &Option<usize>,
    pk_columns: &AVec<ABytes>,
) -> AVec<AutoSet> {
    let mut auto_sets: AVec<AutoSet> = AVec::new();
    for s in constraint_sets.iter() {
        let mut set_cols: AVec<IndexColumn> = AVec::new();
        let mut ok = true;
        for cd in s.columns.iter() {
            let nb = handle_bytes(scratch, cd.name);
            match cols
                .iter()
                .position(|c| eq_ci(handle_bytes(catalog_arena, c.name), nb))
            {
                Some(i) => set_cols.push(IndexColumn {
                    col: if Some(i) == *rowid_alias {
                        None
                    } else {
                        Some(i)
                    },
                    desc: cd.desc,
                    // v0.16: an autoindex keys by the constraint's
                    // explicit COLLATE, else the column's.
                    coll: cd
                        .coll
                        .or_else(|| cols.get(i).map(|c| c.coll))
                        .unwrap_or_default(),
                }),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        // The alias PK set needs no index at all.
        if s.primary_key && rowid_alias.is_some() && pk_columns.len() == 1 {
            continue;
        }
        if ok {
            auto_sets.push(AutoSet {
                primary_key: s.primary_key,
                columns: set_cols,
                #[cfg(feature = "sql")]
                conflict: s.conflict,
            });
        }
    }
    auto_sets
}
