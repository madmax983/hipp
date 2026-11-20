//! Result column names for `.headers`, the json and line modes and
//! `.mode insert`'s column list (v0.17, `shell` unit).
//!
//! The library's `Statement` hands out values, not names, and this
//! unit changes no library code; so the shell names a statement's
//! columns itself, from its parsed text (`hipp::sql::parser`) and the
//! catalog, the way the pin's `generateColumnNames` does under its
//! default `short_column_names`:
//!
//! - an `AS` alias names its column;
//! - an unaliased column reference is named by its source's column —
//!   a table's declared name (`SELECT ab FROM t(Ab)` is `Ab`), a FROM
//!   subquery's, view's or CTE's own column name (made unique with
//!   `:1`, `:2`, ... as the pin's `sqlite3ColumnsFromExprList` makes
//!   them), `rowid` or the INTEGER PRIMARY KEY's name for a rowid;
//! - `*` and `t.*` expand to their sources' column names (a USING or
//!   NATURAL join's shared columns once);
//! - anything else is named by its text as written;
//! - a compound is named by its first core, VALUES by `column1`, ...;
//! - a PRAGMA by its columns (`cid`, `name`, ...) or its own name.
//!
//! Where the text cannot be read (it does not parse), every column is
//! named by its position, `column1`, ... (an honest gap: such a
//! statement fails before it returns a row anyway).

use hipp::Scope;
use hipp::arena::{ABytes, Arena};
use hipp::sql::ast::{Ast, Expr, FromSource, SelectItem, SelectStmt};
use hipp::{Connection, Step};

/// The names of `sql`'s result columns (as many as it names; the
/// caller fits them to the statement's width).
pub fn result_names<S: hipp::Storage>(db: &mut Connection<S>, sql: &str) -> Vec<Vec<u8>>
where
    S::Error: core::fmt::Debug,
{
    let arena = Arena::new(Scope::Statement, 0, 64 * 1024 * 1024);
    let parsed =
        hipp::sql::parser::parse::<()>(sql.as_bytes(), &arena, hipp::sql::parser::Limits::PIN);
    match parsed {
        Ok(ast) => {
            let mut nm = Namer {
                db,
                arena: &arena,
                ctes: Vec::new(),
                depth: 0,
            };
            nm.top(&ast)
        }
        Err(_) => Vec::new(),
    }
}

/// One FROM source as name resolution sees it.
struct Src {
    /// The alias, else the table's name as written.
    key: Vec<u8>,
    cols: Vec<Vec<u8>>,
    /// The INTEGER PRIMARY KEY column a rowid reference is named by.
    ipk: Option<Vec<u8>>,
    using: Vec<Vec<u8>>,
    natural: bool,
}

/// A common table expression in scope: its name, its column list as
/// written, its body.
type Cte<'a> = (Vec<u8>, Vec<Vec<u8>>, &'a SelectStmt);

struct Namer<'a, S: hipp::Storage> {
    db: &'a mut Connection<S>,
    arena: &'a Arena,
    /// The common table expressions in scope, innermost last: name,
    /// column list as written, body.
    ctes: Vec<Cte<'a>>,
    depth: usize,
}

fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn is_rowid(n: &[u8]) -> bool {
    eq_ci(n, b"rowid") || eq_ci(n, b"_rowid_") || eq_ci(n, b"oid")
}

/// The pin's multi-column pragmas' result columns.
fn pragma_columns(name: &[u8]) -> Option<&'static [&'static str]> {
    let n = name.to_ascii_lowercase();
    Some(match n.as_slice() {
        b"table_info" => &["cid", "name", "type", "notnull", "dflt_value", "pk"],
        b"table_xinfo" => &[
            "cid",
            "name",
            "type",
            "notnull",
            "dflt_value",
            "pk",
            "hidden",
        ],
        b"index_list" => &["seq", "name", "unique", "origin", "partial"],
        b"index_info" => &["seqno", "cid", "name"],
        b"index_xinfo" => &["seqno", "cid", "name", "desc", "coll", "key"],
        b"foreign_key_list" => &[
            "id",
            "seq",
            "table",
            "from",
            "to",
            "on_update",
            "on_delete",
            "match",
        ],
        b"table_list" => &["schema", "name", "type", "ncol", "wr", "strict"],
        b"foreign_key_check" => &["table", "rowid", "parent", "fkid"],
        b"database_list" => &["seq", "name", "file"],
        b"wal_checkpoint" => &["busy", "log", "checkpointed"],
        b"collation_list" => &["seq", "name"],
        _ => return None,
    })
}

/// Make names unique as the pin's `sqlite3ColumnsFromExprList` does:
/// a repeat (ignoring case) gets `:1`, `:2`, ... after its name (a
/// `:N` suffix it already has replaced).
fn make_unique(names: &mut [Vec<u8>]) {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    for n in names.iter_mut() {
        let mut cnt = 0u32;
        while seen.iter().any(|s| eq_ci(s, n)) {
            let mut base = n.len();
            if base > 0 {
                let mut j = base - 1;
                while j > 0 && n[j].is_ascii_digit() {
                    j -= 1;
                }
                if n[j] == b':' {
                    base = j;
                }
            }
            cnt += 1;
            let mut v = n[..base].to_vec();
            v.extend_from_slice(format!(":{cnt}").as_bytes());
            *n = v;
        }
        seen.push(n.clone());
    }
}

impl<'a, S: hipp::Storage> Namer<'a, S>
where
    S::Error: core::fmt::Debug,
{
    fn bytes(&self, h: ABytes) -> Vec<u8> {
        self.arena
            .slice_at(h.pos as usize, h.len as usize)
            .unwrap_or(&[])
            .to_vec()
    }

    fn top(&mut self, ast: &'a Ast) -> Vec<Vec<u8>> {
        match ast {
            Ast::Select(sel) => self.select(sel, false),
            Ast::With { with, body } => {
                let mark = self.ctes.len();
                for c in with.ctes.iter() {
                    let cols = c.columns.iter().map(|h| self.bytes(*h)).collect();
                    self.ctes.push((self.bytes(c.name), cols, &c.select));
                }
                let out = self.top(body);
                self.ctes.truncate(mark);
                out
            }
            Ast::Insert {
                table, returning, ..
            } => self.returning(*table, None, returning.as_slice()),
            Ast::Update {
                table,
                alias,
                returning,
                ..
            } => self.returning(*table, *alias, returning.as_slice()),
            Ast::Delete {
                table, returning, ..
            } => self.returning(*table, None, returning.as_slice()),
            Ast::Pragma { name, .. } => {
                let n = self.bytes(*name);
                match pragma_columns(&n) {
                    Some(c) => c.iter().map(|s| s.as_bytes().to_vec()).collect(),
                    None => vec![n.to_ascii_lowercase()],
                }
            }
            _ => Vec::new(),
        }
    }

    /// A RETURNING clause's names, over the target table.
    fn returning(
        &mut self,
        table: ABytes,
        alias: Option<ABytes>,
        items: &[SelectItem],
    ) -> Vec<Vec<u8>> {
        let tname = self.bytes(table);
        let (cols, ipk) = self.table_cols(None, &tname);
        let key = alias.map_or_else(|| tname.clone(), |a| self.bytes(a));
        let srcs = vec![Src {
            key,
            cols,
            ipk,
            using: Vec::new(),
            natural: false,
        }];
        self.items(items, &srcs, false)
    }

    /// A SELECT's names: its first core's (`sub`: as a FROM subquery,
    /// CTE or view body names its columns, made unique).
    fn select(&mut self, sel: &'a SelectStmt, sub: bool) -> Vec<Vec<u8>> {
        self.depth += 1;
        if self.depth > 64 {
            self.depth -= 1;
            return Vec::new();
        }
        let mark = self.ctes.len();
        if let Some(w) = &sel.with {
            for c in w.ctes.iter() {
                let cols = c.columns.iter().map(|h| self.bytes(*h)).collect();
                self.ctes.push((self.bytes(c.name), cols, &c.select));
            }
        }
        let mut out = if let Some(first) = sel.values.as_slice().first() {
            (1..=first.len())
                .map(|i| format!("column{i}").into_bytes())
                .collect()
        } else {
            let srcs = self.sources(sel.from.as_slice());
            self.items(sel.items.as_slice(), &srcs, sub)
        };
        self.ctes.truncate(mark);
        if sub {
            make_unique(&mut out);
        }
        self.depth -= 1;
        out
    }

    fn items(&mut self, items: &[SelectItem], srcs: &[Src], sub: bool) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for it in items {
            match it {
                SelectItem::Star => {
                    for (k, s) in srcs.iter().enumerate() {
                        for c in &s.cols {
                            let shared = s.using.iter().any(|u| eq_ci(u, c))
                                || (s.natural
                                    && srcs[..k]
                                        .iter()
                                        .any(|p| p.cols.iter().any(|pc| eq_ci(pc, c))));
                            if !shared {
                                out.push(c.clone());
                            }
                        }
                    }
                }
                SelectItem::TableStar(t) => {
                    let t = self.bytes(*t);
                    if let Some(s) = srcs.iter().find(|s| eq_ci(&s.key, &t)) {
                        out.extend(s.cols.iter().cloned());
                    }
                }
                SelectItem::Expr(_, Some(alias), _) => out.push(self.bytes(*alias)),
                SelectItem::Expr(e, None, text) => {
                    let col = if sub {
                        match e {
                            Expr::Collate(inner, _) => inner.as_ref(),
                            e => e,
                        }
                    } else {
                        e
                    };
                    let name = match col {
                        Expr::Column { table, name, .. } => {
                            let n = self.bytes(*name);
                            if sub {
                                Some(n)
                            } else {
                                let t = table.map(|t| self.bytes(t));
                                Some(resolve(srcs, t.as_deref(), &n).unwrap_or(n))
                            }
                        }
                        _ => None,
                    };
                    out.push(name.unwrap_or_else(|| self.bytes(*text)));
                }
            }
        }
        out
    }

    fn sources(&mut self, from: &'a [FromSource]) -> Vec<Src> {
        let mut out = Vec::new();
        for f in from {
            let name = self.bytes(f.table);
            let key = f.alias.map_or_else(|| name.clone(), |a| self.bytes(a));
            let using = f.using.iter().map(|u| self.bytes(*u)).collect();
            let (cols, ipk) = if let Some(d) = &f.derived {
                (self.select(d, true), None)
            } else if f.args.is_some() {
                (tvf_cols(&name), None)
            } else {
                let schema = f.schema.map(|s| self.bytes(s));
                let cte = if f.qualified {
                    None
                } else {
                    self.ctes.iter().rposition(|(n, _, _)| eq_ci(n, &name))
                };
                match cte {
                    Some(i) => {
                        let (cols, body) = (self.ctes[i].1.clone(), self.ctes[i].2);
                        if cols.is_empty() {
                            // The CTE's body sees the CTEs before it.
                            let saved = self.ctes.split_off(i);
                            let names = self.select(body, true);
                            self.ctes.extend(saved);
                            (names, None)
                        } else {
                            (cols, None)
                        }
                    }
                    None => self.table_cols(schema.as_deref(), &name),
                }
            };
            out.push(Src {
                key,
                cols,
                ipk,
                using,
                natural: f.natural,
            });
        }
        out
    }

    /// A table's or view's columns (TEMP first, then main, then the
    /// attached databases, as the pin resolves an unqualified name),
    /// and its INTEGER PRIMARY KEY.
    fn table_cols(
        &mut self,
        schema: Option<&[u8]>,
        name: &[u8],
    ) -> (Vec<Vec<u8>>, Option<Vec<u8>>) {
        let q = |s: &[u8]| String::from_utf8_lossy(s).replace('"', "\"\"");
        let mut dbs: Vec<String> = Vec::new();
        match schema {
            Some(s) => dbs.push(q(s)),
            None => {
                if self.db.temp_db().is_some() {
                    dbs.push("temp".into());
                }
                dbs.push("main".into());
                let mut i = 0;
                while let Some((n, _)) = self.db.attached_db(i) {
                    dbs.push(q(n));
                    i += 1;
                }
            }
        }
        for d in dbs {
            let sql = format!("PRAGMA \"{d}\".table_info(\"{}\")", q(name));
            let mut cols = Vec::new();
            let mut pks = Vec::new();
            if let Ok(mut st) = hipp::storage::block_on(self.db.prepare(&sql)) {
                while let Ok(Step::Row) = hipp::storage::block_on(st.step()) {
                    let n = st.column_bytes(1).unwrap_or_default().to_vec();
                    let ty = st.column_bytes(2).unwrap_or_default().to_vec();
                    if st.column_i64(5).unwrap_or(0) > 0 {
                        pks.push((n.clone(), ty));
                    }
                    cols.push(n);
                }
            }
            if cols.is_empty() {
                continue;
            }
            // Generated columns are left out of table_info: take the
            // catalog's full list when it names this table.
            let catalog = match d.as_str() {
                "main" => catalog_cols(self.db, name),
                "temp" => self.db.temp_db().and_then(|t| catalog_cols(t, name)),
                _ => {
                    let mut i = 0;
                    let mut found = None;
                    while let Some((n, a)) = self.db.attached_db(i) {
                        if q(n) == d {
                            found = catalog_cols(a, name);
                            break;
                        }
                        i += 1;
                    }
                    found
                }
            };
            if let Some(c) = catalog {
                cols = c;
            }
            let ipk = match pks.as_slice() {
                [(n, ty)] if eq_ci(ty, b"INTEGER") => Some(n.clone()),
                _ => None,
            };
            return (cols, ipk);
        }
        (Vec::new(), None)
    }
}

/// A table's columns in its database's catalog, generated ones
/// included.
fn catalog_cols<T: hipp::Storage>(c: &Connection<T>, name: &[u8]) -> Option<Vec<Vec<u8>>> {
    c.tables()
        .iter()
        .find(|t| eq_ci(t.name, name))
        .map(|t| t.columns.iter().map(|c| c.to_vec()).collect())
}

/// A table-valued function's visible columns.
fn tvf_cols(name: &[u8]) -> Vec<Vec<u8>> {
    let n = name.to_ascii_lowercase();
    let v: Vec<&str> = match n.as_slice() {
        b"generate_series" => vec!["value"],
        b"json_each" | b"json_tree" => vec![
            "key", "value", "type", "atom", "id", "parent", "fullkey", "path",
        ],
        _ => match n.strip_prefix(b"pragma_") {
            Some(p) => match pragma_columns(p) {
                Some(c) => c.to_vec(),
                None => return vec![p.to_vec()],
            },
            None => Vec::new(),
        },
    };
    v.iter().map(|s| s.as_bytes().to_vec()).collect()
}

/// An unaliased column reference's name: its source's column, or for
/// a rowid the INTEGER PRIMARY KEY's name (`rowid` without one).
fn resolve(srcs: &[Src], table: Option<&[u8]>, name: &[u8]) -> Option<Vec<u8>> {
    let pick: Vec<&Src> = match table {
        Some(t) => srcs.iter().filter(|s| eq_ci(&s.key, t)).collect(),
        None => srcs.iter().collect(),
    };
    for s in &pick {
        if let Some(c) = s.cols.iter().find(|c| eq_ci(c, name)) {
            return Some(c.clone());
        }
    }
    if is_rowid(name) {
        let s = pick.first()?;
        return Some(s.ipk.clone().unwrap_or_else(|| b"rowid".to_vec()));
    }
    None
}
