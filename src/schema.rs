//! Schema catalog (SPEC §5.5): `sqlite_schema` scan → catalog in
//! the catalog arena, built by re-parsing each row's stored SQL
//! with hipp's own parser.
//!
//! Names/decls/SQL live in the catalog arena as `ABytes` handles;
//! `TableDef`s themselves are small metadata in an `AVec` (the
//! arena.rs collections rule, §7.5.1). The catalog is rebuilt whole
//! on a schema-cookie change — its arena's only reset (§7.5.2).

use crate::arena::{ABytes, AVec, Arena};
use crate::btree::ScanCursor;
use crate::error::{Error, Result};
use crate::pager::Pager;
use crate::record::{Affinity, Value, affinity_of, decode_record};
use crate::sql::{Ast, Literal, eq_ci, handle_bytes, parse};
use crate::storage::Storage;

#[derive(Debug, Clone)]
pub struct Column {
    pub name: ABytes,
    pub decl: ABytes,
    pub affinity: Affinity,
    pub default: Option<Literal>,
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
}

pub struct Catalog {
    pub tables: AVec<TableDef>,
    /// Schema cookie this catalog was built from.
    pub cookie: u32,
}

impl Catalog {
    pub fn empty(cookie: u32) -> Catalog {
        Catalog {
            tables: AVec::new(),
            cookie,
        }
    }

    pub fn find<'x>(&'x self, arena: &Arena, name: &[u8]) -> Option<&'x TableDef> {
        self.tables
            .iter()
            .find(|t| eq_ci(handle_bytes(arena, t.name), name))
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

/// Build the catalog by scanning `sqlite_schema` (root page 1).
pub async fn build_catalog<S: Storage>(
    pager: &mut Pager<S>,
    catalog_arena: &Arena,
) -> Result<Catalog, S::Error> {
    let mut catalog = Catalog::empty(pager.schema_cookie());
    let mut cursor = ScanCursor::new(1);
    // Scratch arena for parse trees (names are interned out of it
    // into the catalog arena before the next row).
    let scratch = Arena::new(crate::error::Scope::Statement, 0, 1 << 20);
    while let Some((_rowid, payload)) = cursor.next_row(pager).await? {
        let values = decode_record::<S::Error>(payload.as_slice(), 5)?;
        if values.len() < 5 {
            continue;
        }
        let (Value::Text(ty), Value::Text(name), Value::Integer(root), Value::Text(sql)) = (
            &values.as_slice()[0],
            &values.as_slice()[1],
            &values.as_slice()[3],
            &values.as_slice()[4],
        ) else {
            continue;
        };
        if !eq_ci(ty, b"table") {
            continue; // indexes/views/triggers: not v0.1 objects
        }
        scratch.reset();
        let ast = parse::<S::Error>(sql, &scratch)?;
        let Ast::CreateTable { columns, .. } = ast else {
            return Err(Error::CorruptRecord);
        };
        let mut cols: AVec<Column> = AVec::new();
        let mut pk_count = 0usize;
        let mut alias = None;
        for (i, c) in columns.iter().enumerate() {
            let decl_bytes = handle_bytes(&scratch, c.decl);
            let is_alias = c.primary_key && eq_ci(trim(decl_bytes), b"INTEGER");
            if c.primary_key {
                pk_count += 1;
            }
            if is_alias {
                alias = Some(i);
            }
            cols.push(Column {
                name: intern(catalog_arena, handle_bytes(&scratch, c.name))?,
                decl: intern(catalog_arena, decl_bytes)?,
                affinity: affinity_of(decl_bytes),
                default: match c.default {
                    Some(l) => Some(intern_literal(catalog_arena, l, &scratch)?),
                    None => None,
                },
            });
        }
        // Rowid alias rule: exactly one PK column, of declared type
        // exactly INTEGER (the parser already excluded the
        // `PRIMARY KEY DESC` quirk).
        let rowid_alias = if pk_count == 1 { alias } else { None };
        catalog.tables.push(TableDef {
            name: intern(catalog_arena, name)?,
            root: *root as u32,
            sql: intern(catalog_arena, sql)?,
            columns: cols,
            rowid_alias,
        });
    }
    catalog.cookie = pager.schema_cookie();
    Ok(catalog)
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && (b[s] as char).is_whitespace() {
        s += 1;
    }
    while e > s && (b[e - 1] as char).is_whitespace() {
        e -= 1;
    }
    &b[s..e]
}
