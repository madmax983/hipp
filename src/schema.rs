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
    #[cfg(feature = "v04")]
    pub default_expr: Option<ABytes>,
    /// A generated column's expression text (v0.16).
    #[cfg(feature = "v04")]
    pub gen_expr: Option<ABytes>,
    /// A STORED generated column (v0.16); VIRTUAL otherwise.
    #[cfg(feature = "v04")]
    pub stored: bool,
    /// The column's collating sequence (v0.16; BINARY when no
    /// `COLLATE` clause was written).
    #[cfg(any(feature = "v04", feature = "indexes"))]
    pub coll: Coll,
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
    #[cfg(feature = "v04")]
    pub checks: AVec<ABytes>,
    /// A STRICT table (v0.16, `v16`).
    #[cfg(feature = "v16")]
    pub strict: bool,
    /// The stored SQL names a collating sequence hipp does not
    /// have (an application-registered one, v0.16). The table
    /// is listed (its name is taken, `.schema` shows it) but
    /// has no columns here, and every statement that reads or
    /// writes it is refused with `SchemaKind::UnknownCollation`
    /// (see `TableDef::usable`); the rest of the file works.
    #[cfg(any(feature = "v04", feature = "indexes"))]
    pub foreign_coll: bool,
}

impl TableDef {
    /// The table, unless it names a collating sequence hipp
    /// does not have (v0.16): statements that read or write
    /// such a table are refused with
    /// `SchemaKind::UnknownCollation` — remedy: register no
    /// custom collation, or use an engine that has it.
    pub fn usable<E>(&self) -> Result<&TableDef, E> {
        #[cfg(any(feature = "v04", feature = "indexes"))]
        if self.foreign_coll {
            return Err(Error::Schema {
                kind: crate::error::SchemaKind::UnknownCollation,
            });
        }
        Ok(self)
    }

    /// Whether any column is generated (v0.16).
    #[cfg(feature = "v04")]
    pub fn has_generated(&self) -> bool {
        self.columns.iter().any(|c| c.gen_expr.is_some())
    }

    /// Whether writes must go through table rules (v0.16): a
    /// generated column, a CHECK constraint, or STRICT typing.
    #[cfg(feature = "v04")]
    pub fn has_rules(&self) -> bool {
        #[cfg(feature = "v16")]
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

pub struct Catalog {
    pub tables: AVec<TableDef>,
    pub indexes: AVec<IndexDef>,
    pub views: AVec<ViewDef>,
    /// Schema cookie this catalog was built from.
    pub cookie: u32,
    /// Table and view names in `sqlite_schema` rowid (creation)
    /// order (v0.16, `v16`): the order PRAGMA table_list and
    /// integrity_check walk.
    #[cfg(feature = "v16")]
    pub objects: AVec<ABytes>,
    /// `sqlite_schema` itself as a readable table (v0.16, `v16`):
    /// root page 1, columns `type name tbl_name rootpage sql`.
    #[cfg(feature = "v16")]
    pub schema_table: Option<TableDef>,
}

impl Catalog {
    pub fn empty(cookie: u32) -> Catalog {
        Catalog {
            tables: AVec::new(),
            indexes: AVec::new(),
            views: AVec::new(),
            cookie,
            #[cfg(feature = "v16")]
            objects: AVec::new(),
            #[cfg(feature = "v16")]
            schema_table: None,
        }
    }

    /// The name names `sqlite_schema` (v0.16, `v16`): any case
    /// of `sqlite_schema` / `sqlite_master`.
    #[cfg(feature = "v16")]
    pub fn is_schema_name(name: &[u8]) -> bool {
        eq_ci(name, b"sqlite_schema") || eq_ci(name, b"sqlite_master")
    }

    /// A table to read (v0.16, `v16`): a catalog table, or
    /// `sqlite_schema` under either of its names.
    #[cfg(feature = "v16")]
    pub fn find_readable<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x TableDef> {
        self.find(arena, name).or_else(|| {
            if Catalog::is_schema_name(name) {
                self.schema_table.as_ref()
            } else {
                None
            }
        })
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
            None
        }
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
#[cfg(feature = "v04")]
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

/// Build the catalog by scanning `sqlite_schema` (root page 1).
pub async fn build_catalog<S: Storage>(
    pager: &mut Pager<S>,
    catalog_arena: &Arena,
) -> Result<Catalog, S::Error> {
    // Pass 0: collect the raw schema rows (tables must be built
    // before indexes can resolve their columns).
    let mut rows: AVec<SchemaRow> = AVec::new();
    {
        let mut cursor = ScanCursor::new(1);
        while let Some((_rowid, payload)) = cursor.next_row(pager).await? {
            let values = decode_record::<S::Error>(payload.as_slice(), 5)?;
            if values.len() < 5 {
                continue;
            }
            let (Value::Text(ty), Value::Text(name), Value::Text(tbl_name), Value::Integer(root)) = (
                &values.as_slice()[0],
                &values.as_slice()[1],
                &values.as_slice()[2],
                &values.as_slice()[3],
            ) else {
                continue;
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
        }
    }

    let mut catalog = Catalog::empty(pager.schema_cookie());
    #[cfg(feature = "v16")]
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
        let ast = match parse::<S::Error>(sql.as_slice(), &scratch) {
            Ok(a) => a,
            // v0.16: a collation hipp does not have (one an
            // application registered) — the pin opens such a
            // file and errors only where the collation is used.
            // Keep the table listed but unusable; the rest of
            // the file still opens.
            #[cfg(any(feature = "v04", feature = "indexes"))]
            Err(Error::Schema {
                kind: crate::error::SchemaKind::UnknownCollation,
            }) => {
                catalog.tables.push(TableDef {
                    name: intern(catalog_arena, row.name.as_slice())?,
                    root: row.root,
                    sql: intern(catalog_arena, sql.as_slice())?,
                    columns: AVec::new(),
                    rowid_alias: None,
                    auto_sets: AVec::new(),
                    #[cfg(feature = "v04")]
                    checks: AVec::new(),
                    #[cfg(feature = "v16")]
                    strict: false,
                    foreign_coll: true,
                });
                continue;
            }
            Err(e) => return Err(e),
        };
        let Ast::CreateTable {
            columns,
            pk_columns,
            constraint_sets,
            #[cfg(feature = "v04")]
            checks,
            #[cfg(feature = "v16")]
            strict,
            ..
        } = ast
        else {
            return Err(Error::CorruptRecord);
        };
        let mut cols: AVec<Column> = AVec::new();
        let mut alias = None;
        for (i, c) in columns.iter().enumerate() {
            let decl_bytes = handle_bytes(&scratch, c.decl);
            // Rowid alias rule: exactly one PK column (below), of
            // declared type exactly INTEGER, not written DESC.
            if pk_columns.len() == 1
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
            #[cfg(feature = "v16")]
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
            #[cfg(feature = "v04")]
            let mut default_expr = None;
            #[cfg(feature = "v04")]
            if let Some(h) = c.default_expr {
                let text = handle_bytes(&scratch, h);
                match crate::rules::eval_default::<S::Error>(
                    text,
                    &scratch,
                    &mut crate::rules::Clock::new(None),
                    None,
                ) {
                    Ok(v) => default = Some(owned_literal(catalog_arena, &v)?),
                    Err(_) => default_expr = Some(intern(catalog_arena, text)?),
                }
            }
            cols.push(Column {
                name: intern(catalog_arena, handle_bytes(&scratch, c.name))?,
                decl: intern(catalog_arena, decl_bytes)?,
                affinity,
                default,
                not_null: c.not_null,
                #[cfg(feature = "v04")]
                default_expr,
                #[cfg(feature = "v04")]
                gen_expr: match c.gen_expr {
                    Some(h) => Some(intern(catalog_arena, handle_bytes(&scratch, h))?),
                    None => None,
                },
                #[cfg(feature = "v04")]
                stored: c.stored,
                #[cfg(any(feature = "v04", feature = "indexes"))]
                coll: c.coll.unwrap_or_default(),
            });
        }
        let rowid_alias = alias;
        #[cfg(feature = "indexes")]
        let auto_sets = resolve_auto_sets(
            &scratch,
            catalog_arena,
            cols.as_slice(),
            constraint_sets.as_slice(),
            &rowid_alias,
            &pk_columns,
        );
        #[cfg(not(feature = "indexes"))]
        let auto_sets: AVec<AutoSet> = AVec::new();
        #[cfg(not(feature = "indexes"))]
        let _ = (constraint_sets, pk_columns);
        #[cfg(feature = "v04")]
        let mut check_texts = AVec::new();
        #[cfg(feature = "v04")]
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
            #[cfg(feature = "v04")]
            checks: check_texts,
            #[cfg(feature = "v16")]
            strict,
            #[cfg(any(feature = "v04", feature = "indexes"))]
            foreign_coll: false,
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
            });
            continue;
        }
        #[cfg(feature = "indexes")]
        match &row.sql {
            Some(sql) => {
                // Explicit index: re-parse its CREATE INDEX.
                scratch.reset();
                let parsed = parse::<S::Error>(sql.as_slice(), &scratch);
                let mut idx = IndexDef {
                    name: intern(catalog_arena, row.name.as_slice())?,
                    table: intern(catalog_arena, row.tbl_name.as_slice())?,
                    root: row.root,
                    sql: Some(intern(catalog_arena, sql.as_slice())?),
                    unique: false,
                    origin: IndexOrigin::Explicit,
                    columns: AVec::new(),
                    opaque: false,
                };
                match parsed {
                    Ok(Ast::CreateIndex {
                        unique, columns, ..
                    }) => {
                        idx.unique = unique;
                        let mut ok = true;
                        for cd in columns.iter() {
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
                });
            }
        }
    }
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
        }) = parse::<S::Error>(sql.as_slice(), &scratch)
        {
            columns = derive_view_columns(&catalog, catalog_arena, &scratch, &select, &explicit);
        }
        catalog.views.push(ViewDef {
            name: intern(catalog_arena, row.name.as_slice())?,
            sql: intern(catalog_arena, sql.as_slice())?,
            columns,
        });
    }

    catalog.cookie = pager.schema_cookie();
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
    #[cfg(feature = "v04")]
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
    #[cfg(feature = "v04")]
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
        #[cfg(feature = "v04")]
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
                #[cfg(feature = "v04")]
                let mut joined_it = joined.iter();
                for (_, cols) in sources.iter() {
                    #[cfg(feature = "v04")]
                    let j = joined_it.next();
                    for c in cols.iter() {
                        #[cfg(feature = "v04")]
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
                } else if let crate::sql::Expr::Column { table, name } = e {
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
                        #[cfg(feature = "v04")]
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
    #[cfg(feature = "v04")]
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
            });
        }
    }
    auto_sets
}
