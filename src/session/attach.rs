//! ATTACH / DETACH and statements across several databases (v0.16,
//! `attach`).
//!
//! Each attached database is a `Connection` of its own, of the main
//! connection's storage type, owned by the main one
//! (`Connection::attached`) — the TEMP database's model (`session::
//! temp`) generalised to N databases. Its storage comes from
//! `Connection::set_attach_hook` or, without a hook, from
//! `Storage::open_attached` (`FileStorage` opens files; `MemStorage`
//! makes `:memory:` and temporary databases).
//!
//! Statements are routed before they are planned (`route_attach`),
//! by the pin's name resolution: a schema-qualified name (`aux.t`)
//! is that database's; an unqualified one is the TEMP database's
//! object of the name first, then main's, then each attached
//! database's in attach order.
//!
//! - A statement whose names all live in one attached database runs
//!   there, with that database's qualifiers cut from its text (the
//!   attached connection is its own `main`): DDL, ALTER TABLE,
//!   DROP, VACUUM, REINDEX, ANALYZE, `PRAGMA aux.x`, writes and
//!   queries alike, with that database's indexes, views, triggers and
//!   foreign keys.
//! - A write of an attached table that reads other databases runs on
//!   the attached database too; the tables it reads elsewhere are
//!   handed to it as external sources (`ExtTab`): their columns at
//!   prepare, their rows (read in rowid order from their own
//!   database) when the statement runs. Nothing the statement does can
//!   change those databases (a trigger or foreign-key action stays in
//!   its own database), so the rows read at the start are the rows.
//! - Any other statement naming an attached object (a query or a
//!   write of main reading attached tables) runs on main, where
//!   `ext_source` makes each attached table such a source, read from
//!   its database when the statement scans it.
//! - BEGIN / COMMIT / ROLLBACK / SAVEPOINT / RELEASE / ROLLBACK TO
//!   apply to every database (a database attached inside a
//!   transaction joins it). A COMMIT that wrote more than one
//!   rollback-journal database is made atomic by the pin's
//!   super-journal (`commit_all`; `Pager::commit_phase_one`).

use super::temp::{Kind, apply_cuts, ast_names, tok_index};
use super::*;
use crate::record::{Affinity, Coll};
use crate::sql::lexer::{Tok, Token};
use crate::storage::{AttachTarget, SidecarFile};

/// The pin's `SQLITE_MAX_ATTACHED`.
pub(super) const MAX_ATTACHED: usize = 10;

/// The connection's ATTACH storage hook (`Connection::set_attach_hook`).
pub(super) type AttachHook<S> = Box<dyn FnMut(AttachTarget<'_>) -> Option<S>>;

/// A boxed future (an attached database is a `Connection` too, so
/// calls into it recurse at the type level).
type Boxed<'x, T> = core::pin::Pin<Box<dyn core::future::Future<Output = T> + 'x>>;

/// The rows of a table read from another database: values (defaults
/// materialised, VIRTUAL columns computed, REAL forced) and rowids.
type Rows = AVec<(AVec<OwnedValue>, Option<i64>)>;

/// One attached database.
pub(super) struct Attached<S: Storage> {
    /// Its name as `ATTACH ... AS` gave it (compared without case).
    pub(super) name: AVec<u8>,
    pub(super) conn: Box<Connection<S>>,
    /// The open transaction has read or written it (DETACH's
    /// "database is locked").
    pub(super) used: core::cell::Cell<bool>,
}

/// A table of another database that the current statement reads
/// (`Connection::ext`): what its FROM name was, where it lives, its
/// columns, and — once read — its rows.
pub(super) struct ExtTab {
    /// The FROM name's schema as written (`None`: unqualified).
    schema: Option<AVec<u8>>,
    name: AVec<u8>,
    /// Its database in the main connection's numbering: 0 main,
    /// 1 temp, 2 + i attached database i.
    db: u8,
    cols: AVec<(AVec<u8>, Affinity)>,
    decls: AVec<AVec<u8>>,
    colls: AVec<Coll>,
    rowid_alias: Option<usize>,
    has_rowid: bool,
    rows: Option<Rows>,
}

/// A table the main connection hands an attached database's
/// statement (`OnRoute::ext`).
#[derive(Clone)]
pub(super) struct ExtReq {
    schema: Option<AVec<u8>>,
    name: AVec<u8>,
    db: u8,
}

/// A statement that runs on attached database `db`, as `sql`.
pub(super) struct OnRoute {
    db: usize,
    sql: AVec<u8>,
    ext: AVec<ExtReq>,
    /// Running it inside a transaction locks the database against
    /// DETACH (`Attached::used`): `false` for the pragmas the pin
    /// answers without reading the file (`PRAGMA aux.page_size`, ...).
    lock: bool,
}

/// Where `route_attach` sends a statement that involves an attached
/// database.
pub(super) enum AttRoute {
    /// On main, as written, without the TEMP router (main reads the
    /// attached tables itself).
    Main,
    /// Through the TEMP machinery (a temp write fed from main).
    Temp(super::temp::Route),
    /// On an attached database.
    On(Box<OnRoute>),
    /// Nothing to do (`DROP TABLE IF EXISTS nosuch.t`).
    Nothing,
}

/// A planned statement of an attached database (`Plan::Att`).
pub(super) struct AttPlan {
    pub(super) db: usize,
    pub(super) sql: AVec<u8>,
    pub(super) width: usize,
    pub(super) ext: AVec<ExtReq>,
    /// `OnRoute::lock`.
    lock: bool,
}

/// A planned ATTACH (`file: Some`) or DETACH (`Plan::Attach`).
pub(super) struct AttachPlan {
    file: Option<RExpr>,
    name: RExpr,
    key: Option<RExpr>,
}

/// Where a name lives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loc {
    Main,
    Temp,
    Att(usize),
    /// In no database: it binds where the statement runs.
    Nowhere,
}

/// One table / view / index name of a statement.
struct Ref {
    loc: Loc,
    /// Its token, and its schema qualifier's token.
    tok: Option<usize>,
    schema_tok: Option<usize>,
    name: AVec<u8>,
    schema: Option<AVec<u8>>,
}

/// Whether a FROM / target name of `ast` (parsed with tokens `t`) is
/// qualified with a schema other than `own` — the pin's `DbFixer`
/// rule for a view or trigger stored in database `own`. Table-valued
/// functions count (`main.pragma_table_info(...)`); a column's
/// qualifiers (`aux.t.c`) do not, as on the pin.
fn foreign_names(ast: &Ast, t: &[Token], arena: &Arena, own: &[u8]) -> bool {
    let mut names = AVec::new();
    let mut ctes = AVec::new();
    let _ = ast_names(ast, &mut names, &mut ctes);
    let mut placed: AVec<usize> = AVec::new();
    for (h, kind) in names.iter() {
        if *kind == Kind::Qual {
            continue;
        }
        let n = handle_bytes(arena, *h);
        // A name the parser copied (`x IN aux.t`) is found by its
        // text among the qualified names not yet placed.
        let i = tok_index(t, *h).or_else(|| {
            (0..t.len()).find(|&k| {
                ident(t, k, arena).is_some_and(|y| eq_ci(y, n))
                    && schema_tok(t, k).is_some()
                    && !placed.as_slice().contains(&k)
            })
        });
        if let Some(i) = i {
            placed.push(i);
        }
        if i.and_then(|i| schema_tok(t, i))
            .and_then(|k| ident(t, k, arena))
            .is_some_and(|q| !eq_ci(q, own))
        {
            return true;
        }
    }
    false
}

/// Whether trigger `ct`'s WHEN clause or a statement of its body
/// (`src` is the CREATE TRIGGER text) names an object of a database
/// other than `own` (`foreign_names`). A part that does not parse is
/// left to the statement's own parse to report.
#[cfg(feature = "triggers")]
fn trigger_reaches<E>(
    ct: &crate::sql::ast::CreateTrigger,
    src: &[u8],
    arena: &Arena,
    lim: crate::sql::parser::Limits,
    own: &[u8],
) -> bool {
    let mut parts: AVec<AVec<u8>> = AVec::new();
    if let Some((a, b)) = ct.when {
        let mut w = copy_vec(b"SELECT 1 WHERE ");
        w.extend_from_slice(src.get(a..b).unwrap_or_default());
        parts.push(w);
    }
    for &(a, b) in ct.steps.iter() {
        parts.push(copy_vec(src.get(a..b).unwrap_or_default()));
    }
    parts.iter().any(|p| {
        crate::sql::parser::parse_trigger_step_tokens::<E>(p.as_slice(), arena, lim)
            .is_ok_and(|(ast, toks)| foreign_names(&ast, toks.as_slice(), arena, own))
    })
}

/// The schema qualifier's token before name token `i` (`aux` of
/// `aux.t`): an identifier, then a dot, with no dot before it.
fn schema_tok(t: &[Token], i: usize) -> Option<usize> {
    let dot = i.checked_sub(1)?;
    if t.get(dot)?.tok != Tok::Dot {
        return None;
    }
    let k = dot.checked_sub(1)?;
    match t.get(k)?.tok {
        Tok::Ident(..) => Some(k),
        _ => None,
    }
}

/// `o` as a list of none or one.
fn opt_vec(o: Option<usize>) -> AVec<usize> {
    let mut v = AVec::new();
    if let Some(x) = o {
        v.push(x);
    }
    v
}

/// The text of identifier token `i`.
fn ident<'a>(t: &[Token], i: usize, arena: &'a Arena) -> Option<&'a [u8]> {
    match t.get(i)?.tok {
        Tok::Ident(h, _) => Some(handle_bytes(arena, h)),
        _ => None,
    }
}

/// The value's text, as the pin's `sqlite3_value_text` gives an ATTACH
/// operand: NULL is empty.
fn value_text(v: &OwnedValue) -> AVec<u8> {
    let mut out = AVec::new();
    match v {
        OwnedValue::Null => {}
        OwnedValue::Integer(i) => super::pragma::dec(*i, &mut out),
        OwnedValue::Real(f) => crate::record::render_real(*f, &mut out),
        OwnedValue::Text(b) | OwnedValue::Blob(b) => out.extend_from_slice(b.as_slice()),
    }
    out
}

/// A table's columns as an external source, from its own database's
/// catalog: `None` when it has no table (or schema table) of the name.
fn table_meta<T: Storage>(c: &Connection<T>, name: &[u8]) -> Option<ExtTab> {
    let ca = &c.catalog_arena;
    let t = c.catalog.find_readable(ca, name)?;
    let mut cols = AVec::new();
    let mut decls = AVec::new();
    for col in t.columns.iter() {
        cols.push((copy_vec(handle_bytes(ca, col.name)), col.affinity));
        decls.push(copy_vec(handle_bytes(ca, col.decl)));
    }
    Some(ExtTab {
        schema: None,
        name: copy_vec(name),
        db: 0,
        cols,
        decls,
        colls: super::table_colls(t),
        rowid_alias: t.rowid_alias,
        has_rowid: t.has_rowid(),
        rows: None,
    })
}

/// Every row of table `name` of `c`, as a query reads them (rowid
/// order; defaults materialised, VIRTUAL columns computed, REAL
/// columns forced).
async fn table_rows<T: Storage>(c: &mut Connection<T>, name: &[u8]) -> Result<Rows, T::Error> {
    let arena = Arena::new(Scope::Statement, c.cfg.stmt_init, c.cfg.stmt_max);
    let ca = &c.catalog_arena;
    let Some(t) = c.catalog.find_readable(ca, name) else {
        return Err(Error::Schema {
            kind: SchemaKind::UnknownTable,
        });
    };
    let t = t.usable::<T::Error>()?;
    // A WITHOUT ROWID table comes in key order, under handles that
    // are no rowids.
    let (root, ncols, rowid_alias, has_rowid) =
        (t.root, t.columns.len(), t.rowid_alias, t.has_rowid());
    let mut affs = AVec::new();
    let mut defaults = AVec::new();
    for col in t.columns.iter() {
        affs.push(col.affinity);
        defaults.push(super::read_default(ca, col));
    }
    let gcols = if t.has_generated() {
        c.table_rules(&arena, t)?.and_then(|r| r.gcols)
    } else {
        None
    };
    let mut rows = AVec::new();
    let mut cursor = crate::btree::ScanCursor::new(root);
    let mut buf = AVec::new();
    while let Some(rowid) = cursor.next_row_into(&mut c.pager, &mut buf).await? {
        let virt = gcols.as_ref().map(|g| g.virt.as_slice());
        let mut rec =
            crate::rules::decode_rec::<T::Error>(buf.as_slice(), ncols, defaults.as_slice(), virt)?;
        if let Some(g) = &gcols {
            g.fill::<T::Error>(rec.as_mut_slice(), rowid, true)?;
        }
        let rd = crate::rules::eval_row(rowid_alias, affs.as_slice(), rec.as_slice(), rowid);
        rows.push((rd.vals, has_rowid.then_some(rowid)));
    }
    Ok(rows)
}

/// Run one statement on `c` to completion.
async fn run_on<T: Storage>(c: &mut Connection<T>, sql: &[u8]) -> Result<(), T::Error> {
    let Ok(text) = core::str::from_utf8(sql) else {
        return Err(Error::Unsupported);
    };
    let prep: Boxed<'_, Result<Statement<'_, T>, T::Error>> = Box::pin(c.prepare(text));
    let mut st = prep.await?;
    loop {
        let step: Boxed<'_, Result<Step, T::Error>> = Box::pin(st.step());
        if step.await? == Step::Done {
            return Ok(());
        }
    }
}

impl<S: Storage> Connection<S> {
    /// Set the storage hook ATTACH opens databases with (v0.16,
    /// `attach`): called with what the statement names — a file name
    /// as written, `:memory:` or a temporary database (`''` / NULL) —
    /// it returns the storage, or `None` for the pin's "unable to open
    /// database" (`SchemaKind::CannotOpenDatabase`). Without a hook
    /// the main storage's `Storage::open_attached` is asked, so a
    /// `no_std` caller supplies storage for attached names here (a
    /// flash partition per name, a shared `MemStorage`, ...). The
    /// storage must be of the main database's type; a database's
    /// journal and log are its own sidecars, and a commit across
    /// several of them writes a super-journal through the main
    /// storage's `Storage::open_named`.
    pub fn set_attach_hook(&mut self, hook: impl FnMut(AttachTarget<'_>) -> Option<S> + 'static) {
        self.attach_hook = Some(Box::new(hook));
    }

    /// The attached databases' names, in attach order (v0.16,
    /// `attach`; `PRAGMA database_list` shows them with their files).
    pub fn attached_names(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.attached.iter().map(|a| a.name.as_slice())
    }

    /// Attached database `i` (in attach order) and its name (v0.16,
    /// `attach`): for tools that read its catalog (`tables`,
    /// `view_columns`, ...). `None` past the last.
    pub fn attached_db(&self, i: usize) -> Option<(&[u8], &Connection<S>)> {
        self.attached
            .as_slice()
            .get(i)
            .map(|a| (a.name.as_slice(), &*a.conn))
    }

    /// The attached database named `name` (any case).
    pub(super) fn att_index(&self, name: &[u8]) -> Option<usize> {
        self.attached
            .iter()
            .position(|a| eq_ci(a.name.as_slice(), name))
    }

    /// The name this database goes by in schema-qualified names and
    /// in PRAGMA output: an attached database's own, else `main`.
    pub(super) fn schema_label(&self) -> &[u8] {
        match &self.alias {
            Some(a) => a.as_slice(),
            None => b"main",
        }
    }

    /// Where a name lives: `schema` as written, else the pin's search
    /// order (temp, main, then attached in attach order). `Err(())`
    /// for a schema no database has.
    fn locate(
        &self,
        schema: Option<&[u8]>,
        name: &[u8],
        index: bool,
    ) -> core::result::Result<Loc, ()> {
        if let Some(q) = schema {
            if eq_ci(q, b"main") {
                return Ok(Loc::Main);
            }
            if eq_ci(q, b"temp") {
                return Ok(Loc::Temp);
            }
            return self.att_index(q).map(Loc::Att).ok_or(());
        }
        if self.temp_has(name, index) || super::temp::is_temp_schema_name(name) {
            return Ok(Loc::Temp);
        }
        if self.main_has(name, index) || self.catalog.find_view(&self.catalog_arena, name).is_some()
        {
            return Ok(Loc::Main);
        }
        for (i, a) in self.attached.iter().enumerate() {
            let ca = &a.conn.catalog_arena;
            let has = if index {
                a.conn.catalog.find_index(ca, name).is_some()
            } else {
                a.conn.catalog.taken(ca, name) == Some(false)
            };
            if has {
                return Ok(Loc::Att(i));
            }
        }
        Ok(Loc::Nowhere)
    }

    /// Whether `name` is a view in `loc`.
    fn is_view_in(&self, loc: Loc, name: &[u8]) -> bool {
        match loc {
            Loc::Main => self.catalog.find_view(&self.catalog_arena, name).is_some(),
            Loc::Temp => self.temp_view(name).is_some(),
            Loc::Att(i) => self.attached.as_slice().get(i).is_some_and(|a| {
                a.conn
                    .catalog
                    .find_view(&a.conn.catalog_arena, name)
                    .is_some()
            }),
            Loc::Nowhere => false,
        }
    }

    /// Route `sql` when it involves an attached database (see the
    /// module docs); `None` leaves it to the TEMP router and main.
    pub(super) fn route_attach(
        &self,
        sql: &[u8],
        arena: &Arena,
    ) -> Result<Option<AttRoute>, S::Error> {
        if self.alias.is_some() || self.is_temp {
            return Ok(None);
        }
        let Ok(toks) = crate::sql::parser::tokens::<S::Error>(sql, arena) else {
            return Ok(None);
        };
        let t = toks.as_slice();
        // Nothing attached and no qualifier but main / temp: nothing
        // to route.
        let foreign = (0..t.len()).any(|i| {
            matches!(t.get(i + 1).map(|x| x.tok), Some(Tok::Dot))
                && ident(t, i, arena).is_some_and(|n| !eq_ci(n, b"main") && !eq_ci(n, b"temp"))
        });
        if self.attached.is_empty() && !foreign {
            return Ok(None);
        }
        let word = |i: usize, w: &[u8]| matches!(t.get(i).map(|x| x.tok), Some(Tok::Ident(h, false)) if eq_ci(handle_bytes(arena, h), w));
        let temp_kw = word(0, b"CREATE") && (word(1, b"TEMP") || word(1, b"TEMPORARY"));
        // The statement without its TEMP keyword (as the TEMP router
        // reads it).
        let mut text = AVec::new();
        match (temp_kw, t.get(1), t.get(2)) {
            (true, Some(a), Some(b)) => {
                text.extend_from_slice(sql.get(..a.offset).unwrap_or_default());
                text.extend_from_slice(sql.get(b.offset..).unwrap_or_default());
            }
            _ => text.extend_from_slice(sql),
        }
        let parsed = crate::sql::parser::parse_with_tokens::<S::Error>(
            text.as_slice(),
            arena,
            self.cfg.limits(),
        );
        let Ok((ast, toks)) = parsed else {
            return Ok(None);
        };
        let t = toks.as_slice();
        let src = text.as_slice();
        let body = match &ast {
            Ast::With { body, .. } => body.as_ref(),
            a => a,
        };
        let att_name = |i: usize| self.attached.as_slice().get(i).map(|a| a.name.as_slice());
        // The text with the given qualifier tokens (`aux.`) cut.
        let cut = |ks: &[usize]| {
            let mut cuts = AVec::new();
            for &k in ks {
                if let (Some(a), Some(b)) = (t.get(k), t.get(k + 2)) {
                    cuts.push((a.offset, b.offset));
                }
            }
            apply_cuts(src, &mut cuts)
        };
        let on = |db: usize, sql: AVec<u8>, ext: AVec<ExtReq>| {
            Ok(Some(AttRoute::On(Box::new(OnRoute {
                db,
                sql,
                ext,
                lock: true,
            }))))
        };
        match body {
            Ast::Attach { .. }
            | Ast::Detach { .. }
            | Ast::Begin
            | Ast::BeginImmediate
            | Ast::Commit
            | Ast::Rollback
            | Ast::Savepoint { .. }
            | Ast::Release { .. }
            | Ast::RollbackTo { .. } => return Ok(None),
            Ast::Pragma {
                schema,
                name,
                value,
                ..
            } => {
                let p = super::pragma::lookup::<S::Error>(handle_bytes(arena, *name))
                    .ok()
                    .flatten();
                if p.is_some_and(|p| p.connection_wide() || p == super::pragma::Prag::DatabaseList)
                {
                    return Ok(None);
                }
                return match schema {
                    Some(h) => {
                        let q = handle_bytes(arena, *h);
                        if eq_ci(q, b"main") || eq_ci(q, b"temp") {
                            return Ok(None);
                        }
                        let Some(i) = self.att_index(q) else {
                            return Err(Error::Schema {
                                kind: SchemaKind::UnknownDatabase,
                            });
                        };
                        let k = tok_index(t, *h).unwrap_or(0);
                        // The pin reads no page for these (v0.16 review
                        // fix: DETACH inside a transaction succeeds).
                        let lock = !p.is_some_and(|p| p.no_lock());
                        Ok(Some(AttRoute::On(Box::new(OnRoute {
                            db: i,
                            sql: cut(&[k]),
                            ext: AVec::new(),
                            lock,
                        }))))
                    }
                    None => {
                        let Some(v) = value else {
                            return Ok(None);
                        };
                        let n = handle_bytes(arena, *v);
                        match self.locate(None, n, false) {
                            Ok(Loc::Att(i)) => on(i, copy_vec(src), AVec::new()),
                            _ => match self.locate(None, n, true) {
                                Ok(Loc::Att(i)) => on(i, copy_vec(src), AVec::new()),
                                _ => Ok(None),
                            },
                        }
                    }
                };
            }
            Ast::Vacuum { schema, .. } => {
                let Some(h) = schema else {
                    return Ok(None);
                };
                let Some(i) = self.att_index(handle_bytes(arena, *h)) else {
                    return Ok(None);
                };
                // `VACUUM aux [INTO x]` is `VACUUM [INTO x]` there.
                let k = tok_index(t, *h).unwrap_or(0);
                let mut cuts = AVec::new();
                if let (Some(a), Some(b)) = (t.get(k), t.get(k + 1)) {
                    cuts.push((a.offset, b.offset));
                } else if let Some(a) = t.get(k) {
                    cuts.push((a.offset, src.len()));
                }
                return on(i, apply_cuts(src, &mut cuts), AVec::new());
            }
            Ast::Reindex { schema, name } | Ast::Analyze { schema, name } => {
                let analyze = matches!(body, Ast::Analyze { .. });
                if let Some(h) = schema {
                    let q = handle_bytes(arena, *h);
                    if let Some(i) = self.att_index(q) {
                        let k = tok_index(t, *h).unwrap_or(0);
                        return on(i, cut(&[k]), AVec::new());
                    }
                    return Ok(None);
                }
                let Some(n) = name.map(|h| handle_bytes(arena, h)) else {
                    return Ok(None);
                };
                // `ANALYZE aux` analyzes that database.
                if analyze && let Some(i) = self.att_index(n) {
                    return on(i, copy_vec(b"ANALYZE"), AVec::new());
                }
                if !analyze && crate::record::Coll::named(n).is_some() {
                    return Ok(None);
                }
                for index in [false, true] {
                    if let Ok(Loc::Att(i)) = self.locate(None, n, index) {
                        return on(i, copy_vec(src), AVec::new());
                    }
                }
                return Ok(None);
            }
            #[cfg(feature = "triggers")]
            Ast::CreateTrigger(ct) => return self.route_trigger(ct, temp_kw, t, src, arena),
            #[cfg(feature = "triggers")]
            Ast::DropTrigger { name, if_exists } => {
                let n = handle_bytes(arena, *name);
                let k = tok_index(t, *name).and_then(|i| schema_tok(t, i));
                match k.and_then(|k| ident(t, k, arena)) {
                    Some(q) if eq_ci(q, b"main") || eq_ci(q, b"temp") => return Ok(None),
                    Some(q) => {
                        return match self.att_index(q) {
                            Some(i) => on(i, cut(&[k.unwrap_or(0)]), AVec::new()),
                            None if *if_exists => Ok(Some(AttRoute::Nothing)),
                            None => Err(Error::Schema {
                                kind: SchemaKind::UnknownTrigger,
                            }),
                        };
                    }
                    None => {}
                }
                let in_main_or_temp = self.catalog.find_trigger(&self.catalog_arena, n).is_some()
                    || self
                        .temp
                        .as_ref()
                        .is_some_and(|c| c.catalog.find_trigger(&c.catalog_arena, n).is_some());
                if in_main_or_temp {
                    return Ok(None);
                }
                for (i, a) in self.attached.iter().enumerate() {
                    if a.conn
                        .catalog
                        .find_trigger(&a.conn.catalog_arena, n)
                        .is_some()
                    {
                        return on(i, copy_vec(src), AVec::new());
                    }
                }
                return Ok(None);
            }
            _ => {}
        }
        // Every table / view / index name of the statement.
        let mut names = AVec::new();
        let mut ctes = AVec::new();
        let created = ast_names(&ast, &mut names, &mut ctes);
        let mut refs: AVec<Ref> = AVec::new();
        let mut placed: AVec<usize> = AVec::new();
        for (h, kind) in names.iter() {
            if matches!(kind, Kind::Qual | Kind::Func) {
                continue;
            }
            let n = handle_bytes(arena, *h);
            // A name the parser copied (`x IN aux.t`) is found by its
            // text among the qualified names not yet placed.
            let i = tok_index(t, *h).or_else(|| {
                (0..t.len()).find(|&k| {
                    ident(t, k, arena).is_some_and(|y| eq_ci(y, n))
                        && schema_tok(t, k).is_some()
                        && !placed.as_slice().contains(&k)
                })
            });
            if let Some(i) = i {
                placed.push(i);
            }
            let sk = i.and_then(|i| schema_tok(t, i));
            let schema = sk.and_then(|k| ident(t, k, arena));
            if schema.is_none() && ctes.iter().any(|c| eq_ci(handle_bytes(arena, *c), n)) {
                continue;
            }
            // A table-valued pragma function is main's, whatever its
            // qualifier (the pin ignores it).
            if n.get(..7).is_some_and(|p| eq_ci(p, b"pragma_"))
                && !self.catalog.taken(&self.catalog_arena, n).is_some()
            {
                continue;
            }
            // An attached database has no TEMP schema table.
            if schema.is_some_and(|q| self.att_index(q).is_some())
                && super::temp::is_temp_schema_name(n)
            {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            }
            let loc = match self.locate(schema, n, *kind == Kind::Index) {
                Ok(l) => l,
                Err(()) => {
                    let if_exists = matches!(
                        body,
                        Ast::DropTable {
                            if_exists: true,
                            ..
                        } | Ast::DropView {
                            if_exists: true,
                            ..
                        } | Ast::DropIndex {
                            if_exists: true,
                            ..
                        }
                    );
                    if if_exists {
                        return Ok(Some(AttRoute::Nothing));
                    }
                    return Err(Error::Schema {
                        kind: if *kind == Kind::Index {
                            SchemaKind::UnknownIndex
                        } else {
                            SchemaKind::UnknownTable
                        },
                    });
                }
            };
            refs.push(Ref {
                loc,
                tok: i,
                schema_tok: sk,
                name: copy_vec(n),
                schema: schema.map(copy_vec),
            });
        }
        // Three-part column names `aux.t.c`: their schema token.
        let mut three: AVec<(usize, Loc)> = AVec::new();
        for k in 0..t.len() {
            let pat = matches!(
                (
                    t.get(k + 1).map(|x| x.tok),
                    t.get(k + 2).map(|x| x.tok),
                    t.get(k + 3).map(|x| x.tok),
                    t.get(k + 4).map(|x| x.tok)
                ),
                (
                    Some(Tok::Dot),
                    Some(Tok::Ident(..)),
                    Some(Tok::Dot),
                    Some(Tok::Ident(..))
                )
            ) && !(k > 0 && matches!(t.get(k - 1).map(|x| x.tok), Some(Tok::Dot)));
            if !pat || placed.as_slice().contains(&(k + 2)) {
                continue;
            }
            if let Some(q) = ident(t, k, arena) {
                let loc = if eq_ci(q, b"main") {
                    Loc::Main
                } else if eq_ci(q, b"temp") {
                    Loc::Temp
                } else {
                    self.att_index(q).map_or(Loc::Nowhere, Loc::Att)
                };
                three.push((k, loc));
            }
        }
        // The created object's database.
        let mut created_loc = None;
        let mut created_tok = None;
        if let Some((h, is_index)) = created {
            let sk = tok_index(t, h).and_then(|i| schema_tok(t, i));
            created_tok = sk;
            let loc = match sk.and_then(|k| ident(t, k, arena)) {
                Some(q) if eq_ci(q, b"main") => Loc::Main,
                Some(q) if eq_ci(q, b"temp") => Loc::Temp,
                Some(q) => match self.att_index(q) {
                    Some(i) => Loc::Att(i),
                    None => {
                        return Err(Error::Schema {
                            kind: SchemaKind::UnknownDatabase,
                        });
                    }
                },
                None if temp_kw => Loc::Temp,
                // An index of a TEMP table is the temp database's
                // (the TEMP router decides).
                None if is_index => Loc::Main,
                None => Loc::Main,
            };
            created_loc = Some(loc);
        }
        // A main view may not name another database's objects with a
        // qualifier (table-valued pragma functions included; three-part
        // column names are not checked, as on the pin); unqualified,
        // they bind to main (stored, and read later as the pin does).
        if matches!(body, Ast::CreateView { .. })
            && created_loc == Some(Loc::Main)
            && foreign_names(&ast, t, arena, b"main")
        {
            return Err(Error::Schema {
                kind: SchemaKind::CrossDatabaseReference,
            });
        }
        let any_att = refs.iter().any(|r| matches!(r.loc, Loc::Att(_)))
            || matches!(created_loc, Some(Loc::Att(_)))
            || three.iter().any(|x| matches!(x.1, Loc::Att(_)));
        if !any_att {
            return Ok(None);
        }
        // The statement's target (written database) and home.
        let target: Option<&[u8]> = match body {
            Ast::Insert { table, .. }
            | Ast::Update { table, .. }
            | Ast::Delete { table, .. }
            | Ast::AlterAddColumn { table, .. }
            | Ast::AlterRename { table, .. }
            | Ast::AlterRenameColumn { table, .. }
            | Ast::AlterDropColumn { table, .. } => Some(handle_bytes(arena, *table)),
            Ast::DropTable { name, .. }
            | Ast::DropView { name, .. }
            | Ast::DropIndex { name, .. } => Some(handle_bytes(arena, *name)),
            _ => None,
        };
        // The target is the first name `ast_names` lists.
        let target_ref = target.and_then(|_| refs.as_slice().first());
        let home = match (created_loc, target_ref, body) {
            (Some(l), _, _) => l,
            (None, Some(r), _) => r.loc,
            (None, None, Ast::Select(_)) => {
                let mut dbs = refs
                    .iter()
                    .map(|r| r.loc)
                    .chain(three.iter().map(|x| x.1))
                    .filter(|l| *l != Loc::Nowhere);
                match dbs.next() {
                    Some(Loc::Att(i)) if dbs.all(|l| l == Loc::Att(i)) => Loc::Att(i),
                    _ => Loc::Main,
                }
            }
            _ => return Ok(None),
        };
        match home {
            Loc::Main | Loc::Nowhere => {
                if matches!(body, Ast::CreateView { .. }) {
                    // Checked above.
                    return Ok(None);
                }
                if matches!(
                    body,
                    Ast::CreateIndex { .. }
                        | Ast::DropTable { .. }
                        | Ast::DropView { .. }
                        | Ast::DropIndex { .. }
                        | Ast::AlterAddColumn { .. }
                        | Ast::AlterRename { .. }
                        | Ast::AlterRenameColumn { .. }
                        | Ast::AlterDropColumn { .. }
                ) {
                    return Ok(None);
                }
                // A query, or a write of main, reading attached
                // tables: main reads them (`ext_source`).
                Ok(Some(AttRoute::Main))
            }
            Loc::Temp => {
                if matches!(body, Ast::CreateView { .. }) {
                    return Ok(None);
                }
                // A temp write fed by a SELECT main plans (it reads the
                // attached tables): INSERT INTO temp.x SELECT ..., CREATE
                // TEMP TABLE x AS SELECT ....
                let create_temp = created_loc == Some(Loc::Temp);
                match self.fed_route(&ast, &text, t, create_temp, arena)? {
                    Some(f) => Ok(Some(AttRoute::Temp(super::temp::Route::TempFed(Box::new(
                        f,
                    ))))),
                    None => Err(Error::Unsupported),
                }
            }
            Loc::Att(i) => {
                if temp_kw {
                    return Err(Error::Schema {
                        kind: SchemaKind::UnknownDatabase,
                    });
                }
                let own = |r: &Ref| r.loc == Loc::Att(i) || r.loc == Loc::Nowhere;
                match body {
                    Ast::CreateView { .. } => {
                        // Its names bind to its own database; a
                        // qualifier naming another is the pin's error.
                        if foreign_names(&ast, t, arena, att_name(i).unwrap_or_default()) {
                            return Err(Error::Schema {
                                kind: SchemaKind::CrossDatabaseReference,
                            });
                        }
                        let ks: AVec<usize> = opt_vec(created_tok);
                        return on(i, cut(ks.as_slice()), AVec::new());
                    }
                    Ast::CreateIndex { table, .. } => {
                        // `CREATE INDEX aux.i ON aux.t` is the pin's
                        // syntax error.
                        if let Some(k) = tok_index(t, *table)
                            && schema_tok(t, k).is_some()
                        {
                            return Err(Error::Parse {
                                offset: t.get(k).map_or(0, |x| x.offset),
                                kind: crate::error::ParseKind::UnexpectedToken,
                            });
                        }
                        let ks: AVec<usize> = opt_vec(created_tok);
                        return on(i, cut(ks.as_slice()), AVec::new());
                    }
                    _ => {}
                }
                // The qualifiers to cut: every `aux.` of a name of this
                // database, and of a three-part name.
                let mut ks: AVec<usize> = opt_vec(created_tok);
                for r in refs.iter() {
                    if r.loc == Loc::Att(i)
                        && let Some(k) = r.schema_tok
                    {
                        ks.push(k);
                    }
                }
                // This database's three-part names lose their schema
                // there (its sources are its own); one of an unknown
                // schema stays, and fails there as on the pin.
                for x in three.iter() {
                    if x.1 == Loc::Att(i) {
                        ks.push(x.0);
                    }
                }
                let foreign3 = three
                    .iter()
                    .any(|x| x.1 != Loc::Att(i) && x.1 != Loc::Nowhere);
                if refs.iter().all(own) && !foreign3 {
                    return on(i, cut(ks.as_slice()), AVec::new());
                }
                // Reads of other databases: handed over as external
                // sources. A three-part name of another database, a
                // view elsewhere, or an unqualified name that is both
                // this database's and another's are not modelled.
                if foreign3 {
                    return Err(Error::Unsupported);
                }
                let mut ext = AVec::new();
                for r in refs.iter().filter(|r| !own(r)) {
                    if self.is_view_in(r.loc, r.name.as_slice()) {
                        return Err(Error::Unsupported);
                    }
                    if r.schema.is_none()
                        && refs.iter().any(|o| {
                            o.loc == Loc::Att(i) && eq_ci(o.name.as_slice(), r.name.as_slice())
                        })
                    {
                        return Err(Error::Unsupported);
                    }
                    let db = match r.loc {
                        Loc::Main => 0,
                        Loc::Temp => 1,
                        Loc::Att(j) => u8::try_from(j + 2).unwrap_or(u8::MAX),
                        Loc::Nowhere => continue,
                    };
                    let _ = r.tok;
                    ext.push(ExtReq {
                        schema: r.schema.clone(),
                        name: r.name.clone(),
                        db,
                    });
                }
                on(i, cut(ks.as_slice()), ext)
            }
        }
    }

    /// Route a CREATE TRIGGER that involves an attached database: one
    /// named `aux.tr` lives there (its ON table must be there too);
    /// a main or TEMP trigger may not name another database's table.
    #[cfg(feature = "triggers")]
    fn route_trigger(
        &self,
        ct: &crate::sql::ast::CreateTrigger,
        temp_kw: bool,
        t: &[Token],
        src: &[u8],
        arena: &Arena,
    ) -> Result<Option<AttRoute>, S::Error> {
        let nk = tok_index(t, ct.name).and_then(|i| schema_tok(t, i));
        let tk = tok_index(t, ct.table).and_then(|i| schema_tok(t, i));
        let nq = nk.and_then(|k| ident(t, k, arena));
        let tq = tk.and_then(|k| ident(t, k, arena));
        let n_att = nq.and_then(|q| self.att_index(q));
        let t_att = tq.and_then(|q| self.att_index(q));
        if let Some(q) = nq
            && n_att.is_none()
            && !eq_ci(q, b"main")
            && !eq_ci(q, b"temp")
        {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownDatabase,
            });
        }
        let Some(i) = n_att else {
            let temp = temp_kw || nq.is_some_and(|q| eq_ci(q, b"temp"));
            // A main or TEMP trigger on an attached table.
            if t_att.is_some() {
                if temp {
                    return Err(Error::Unsupported);
                }
                return Err(Error::Schema {
                    kind: SchemaKind::CrossDatabaseReference,
                });
            }
            // A main trigger (not one on a TEMP table, which is the
            // temp database's) may name only main's objects, in its
            // WHEN clause as in its body (v0.16 review fix).
            let on_temp = tq.is_some_and(|q| eq_ci(q, b"temp"))
                || (tq.is_none()
                    && !nq.is_some_and(|q| eq_ci(q, b"main"))
                    && self.temp_has(handle_bytes(arena, ct.table), false));
            if !temp
                && !on_temp
                && trigger_reaches::<S::Error>(ct, src, arena, self.cfg.limits(), b"main")
            {
                return Err(Error::Schema {
                    kind: SchemaKind::CrossDatabaseReference,
                });
            }
            return Ok(None);
        };
        if temp_kw {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownDatabase,
            });
        }
        if tq.is_some() && t_att != Some(i) {
            return Err(Error::Schema {
                kind: SchemaKind::CrossDatabaseReference,
            });
        }
        // The WHEN clause and the body may not name another database
        // (the pin's "trigger X cannot reference objects in database
        // Y"; v0.16 review fix: the WHEN clause was not checked).
        let own = self
            .attached
            .as_slice()
            .get(i)
            .map_or(&[][..], |a| a.name.as_slice());
        if trigger_reaches::<S::Error>(ct, src, arena, self.cfg.limits(), own) {
            return Err(Error::Schema {
                kind: SchemaKind::CrossDatabaseReference,
            });
        }
        let mut cuts = AVec::new();
        if let Some(k) = nk
            && let (Some(a), Some(b)) = (t.get(k), t.get(k + 2))
        {
            cuts.push((a.offset, b.offset));
        }
        Ok(Some(AttRoute::On(Box::new(OnRoute {
            db: i,
            sql: apply_cuts(src, &mut cuts),
            ext: AVec::new(),
            lock: true,
        }))))
    }

    /// The external sources' columns, installed in attached database
    /// `db` for its next prepare (rows `None`) or execution.
    pub(super) fn ext_tabs(&self, reqs: &[ExtReq]) -> Result<AVec<ExtTab>, S::Error> {
        let mut out = AVec::new();
        for r in reqs {
            let n = r.name.as_slice();
            let meta = match r.db {
                0 => table_meta(self, n),
                1 => self.temp.as_deref().and_then(|c| table_meta(c, n)),
                d => self
                    .attached
                    .as_slice()
                    .get(usize::from(d).saturating_sub(2))
                    .and_then(|a| table_meta(&a.conn, n)),
            };
            let Some(mut m) = meta else {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            };
            m.schema = r.schema.clone();
            m.db = r.db;
            out.push(m);
        }
        Ok(out)
    }

    /// Every row of table `name` of database `db` (main's numbering).
    async fn ext_rows(&mut self, db: u8, name: &[u8]) -> Result<Rows, S::Error> {
        match db {
            0 => {
                let f: Boxed<'_, Result<Rows, S::Error>> = Box::pin(table_rows(self, name));
                f.await
            }
            1 => match self.temp.as_deref_mut() {
                Some(c) => table_rows(c, name).await.map_err(super::temp::mem_err),
                None => Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                }),
            },
            d => {
                let i = usize::from(d).saturating_sub(2);
                if self.explicit_txn
                    && let Some(a) = self.attached.as_mut_slice().get_mut(i)
                {
                    a.used.set(true);
                }
                match self.attached.as_mut_slice().get_mut(i) {
                    Some(a) => {
                        let f: Boxed<'_, Result<Rows, S::Error>> =
                            Box::pin(table_rows(&mut a.conn, name));
                        f.await
                    }
                    None => Err(Error::Schema {
                        kind: SchemaKind::UnknownTable,
                    }),
                }
            }
        }
    }

    /// A FROM source of another database (see the module docs): the
    /// index of its `ExtTab`, registered on main as the planner meets
    /// it, given by main to an attached database. `None` for a source
    /// of this database (or the TEMP database's, which main reads
    /// through `plan_temp_source`).
    pub(super) fn ext_source(
        &self,
        s: &crate::sql::FromSource,
        arena: &Arena,
    ) -> Result<Option<usize>, S::Error> {
        let name = handle_bytes(arena, s.table);
        let schema = s.schema.map(|h| handle_bytes(arena, h));
        let unknown = || {
            Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            })
        };
        if self.alias.is_some() || self.is_temp {
            let ext = self.ext.borrow();
            let k = ext.iter().position(|e| {
                eq_ci(e.name.as_slice(), name)
                    && match (&e.schema, schema) {
                        (None, None) => true,
                        (Some(a), Some(b)) => eq_ci(a.as_slice(), b),
                        _ => false,
                    }
            });
            if k.is_some() {
                return Ok(k);
            }
            // Its own qualifier (a stored view's `aux.t`) and `main.`
            // are this database's; any other names none of its tables.
            if let Some(q) = schema
                && !eq_ci(q, b"main")
                && !(self.is_temp && eq_ci(q, b"temp"))
                && !self.alias.as_ref().is_some_and(|a| eq_ci(a.as_slice(), q))
            {
                return unknown();
            }
            return Ok(None);
        }
        let i =
            match schema {
                Some(q) if eq_ci(q, b"main") || eq_ci(q, b"temp") => return Ok(None),
                Some(q) => match self.att_index(q) {
                    Some(i) => i,
                    None => return unknown(),
                },
                // A main view's or trigger's name is main's (the
                // expander marked it qualified, `fix_main`).
                None if s.qualified => return Ok(None),
                None => {
                    let ca = &self.catalog_arena;
                    if self.temp_has(name, false)
                        || self.catalog.find_readable(ca, name).is_some()
                        || self.catalog.find_view(ca, name).is_some()
                    {
                        return Ok(None);
                    }
                    match self.attached.iter().position(|a| {
                        a.conn.catalog.taken(&a.conn.catalog_arena, name) == Some(false)
                    }) {
                        Some(i) => i,
                        None => return Ok(None),
                    }
                }
            };
        let Some(a) = self.attached.as_slice().get(i) else {
            return unknown();
        };
        let Some(mut m) = table_meta(&a.conn, name) else {
            if a.conn
                .catalog
                .find_view(&a.conn.catalog_arena, name)
                .is_some()
            {
                // An attached view read by a statement of another
                // database is not expanded here.
                return Err(Error::Unsupported);
            }
            return unknown();
        };
        m.schema = schema.map(copy_vec);
        m.db = u8::try_from(i + 2).unwrap_or(u8::MAX);
        let mut ext = self.ext.borrow_mut();
        ext.push(m);
        Ok(Some(ext.len() - 1))
    }

    /// Plan external source `k` (`ext_source`) as a FROM source.
    pub(super) fn plan_ext_source(
        &self,
        s: &crate::sql::FromSource,
        k: usize,
        arena: &Arena,
        descs: &mut AVec<crate::query::SourceDesc>,
        plans: &mut AVec<super::SourcePlan>,
    ) -> Result<usize, S::Error> {
        let ext = self.ext.borrow();
        let Some(e) = ext.as_slice().get(k) else {
            return Err(Error::Unsupported);
        };
        // `INDEXED BY` names an index of that database, which this
        // read does not use (its rows come in rowid order).
        let qual = match s.alias {
            Some(a) => copy_vec(handle_bytes(arena, a)),
            None => copy_vec(handle_bytes(arena, s.table)),
        };
        let mut desc =
            crate::query::SourceDesc::new(qual, e.cols.clone(), e.rowid_alias, e.has_rowid);
        desc.kind = s.kind;
        desc.colls = e.colls.clone();
        // A three-part name reaches an attached database's table on
        // main by that database's name.
        if self.alias.is_none() && e.db >= 2 {
            desc.db = e.db.saturating_add(2);
            if let Some(a) = self.attached.as_slice().get(usize::from(e.db) - 2) {
                desc.dbname = a.name.clone();
            }
        }
        descs.push(desc);
        let mut affs = AVec::new();
        let mut defaults = AVec::new();
        for (_, a) in e.cols.iter() {
            affs.push(*a);
            defaults.push(OwnedValue::Null);
        }
        plans.push(super::SourcePlan {
            #[cfg(feature = "explain")]
            eqp: Default::default(),
            root: 0,
            kind: s.kind,
            on: None,
            rowid_alias: e.rowid_alias,
            ncols: e.cols.len(),
            affs,
            defaults,
            derived: None,
            gcols: None,
            rec: 0,
            #[cfg(any(feature = "json", feature = "vtab"))]
            tvf: 0,
            #[cfg(any(feature = "json", feature = "vtab"))]
            tvf_dead: AVec::new(),
            #[cfg(feature = "vtab")]
            vt: None,
            #[cfg(feature = "expr_index")]
            access: None,
            #[cfg(feature = "expr_index")]
            hint: crate::query::Hint::Any,
            temp: true,
            ext: u16::try_from(k + 1).unwrap_or(u16::MAX),
            #[cfg(feature = "planner")]
            exists: false,
        });
        Ok(e.cols.len())
    }

    /// Column `c` of external source `ext` (1-based): its name and
    /// declared type (CREATE TABLE ... AS SELECT).
    pub(super) fn ext_col(&self, ext: u16, c: usize) -> Option<(AVec<u8>, AVec<u8>)> {
        let x = self.ext.borrow();
        let e = x.as_slice().get(usize::from(ext).checked_sub(1)?)?;
        Some((
            e.cols.as_slice().get(c)?.0.clone(),
            e.decls.as_slice().get(c)?.clone(),
        ))
    }

    /// Give the attached databases the connection's settings (as
    /// `sync_temp` does for the TEMP database).
    pub(super) fn sync_att(&mut self) {
        let (p, like) = (&self.prag, self.env.case_like.get());
        #[cfg(feature = "utf16")]
        let enc = self.pager.text_enc();
        for a in self.attached.as_mut_slice().iter_mut() {
            let c = &mut a.conn;
            c.prag.foreign_keys = p.foreign_keys;
            c.prag.recursive_triggers = p.recursive_triggers;
            c.prag.ignore_checks = p.ignore_checks;
            c.prag.busy_timeout = p.busy_timeout;
            c.prag.like_redefined = p.like_redefined;
            c.env.case_like.set(like);
            #[cfg(feature = "utf16")]
            if c.pager.text_enc() != enc && c.pager.enc_open() {
                c.set_text_enc(enc);
            }
        }
    }

    /// Apply a transaction statement that succeeded on main to every
    /// attached database too; once no transaction is open, none of
    /// them counts as used.
    pub(super) async fn mirror_att(&mut self, sql: &[u8]) {
        for a in self.attached.as_mut_slice().iter_mut() {
            let r: Boxed<'_, Result<(), S::Error>> = Box::pin(run_on(&mut a.conn, sql));
            let _ = r.await;
        }
        self.att_txn_ended();
    }

    /// Run `sql` (a plain `ANALYZE` / `REINDEX` of main;
    /// `Statement::step_maint`) on every attached database, in attach
    /// order. Each one counts as used by an open transaction.
    pub(super) async fn maint_attached(&mut self, sql: &[u8]) -> Result<(), S::Error> {
        if self.alias.is_some() || self.is_temp {
            return Ok(());
        }
        let txn = self.explicit_txn;
        for a in self.attached.as_mut_slice().iter_mut() {
            if txn {
                a.used.set(true);
            }
            let r: Boxed<'_, Result<(), S::Error>> = Box::pin(run_on(&mut a.conn, sql));
            r.await?;
        }
        Ok(())
    }

    /// Forget which attached databases the transaction used, when none
    /// is open.
    pub(super) fn att_txn_ended(&mut self) {
        if !self.explicit_txn {
            for a in self.attached.as_mut_slice().iter_mut() {
                a.used.set(false);
            }
        }
    }

    /// Plan ATTACH / DETACH: the operands are resolved against no
    /// table (subqueries allowed), evaluated when it runs.
    pub(super) fn plan_attach(&self, arena: &Arena, ast: &Ast) -> Result<Plan, S::Error> {
        use crate::query::{ScopeGroup, has_agg, resolve_scoped};
        let res = |e: &Expr| -> Result<RExpr, S::Error> {
            let cur = ScopeGroup::new(AVec::new(), AVec::new(), 0);
            let mut planner = self.sub_planner(arena);
            let re = resolve_scoped::<S::Error>(e, arena, &cur, &[], &mut planner)?;
            if has_agg(&re) {
                return Err(Error::Unsupported);
            }
            Ok(re)
        };
        let p = match ast {
            Ast::Attach { file, name, key } => AttachPlan {
                file: Some(res(file)?),
                name: res(name)?,
                key: match key {
                    Some(k) => Some(res(k)?),
                    None => None,
                },
            },
            Ast::Detach { name } => AttachPlan {
                file: None,
                name: res(name)?,
                key: None,
            },
            _ => return Err(Error::Unsupported),
        };
        Ok(Plan::Attach(Box::new(p)))
    }

    /// ATTACH `file` AS `name` (the pin's checks in its order: the
    /// limit, the name, then the open and the encoding).
    async fn attach_db(&mut self, file: &[u8], name: &[u8]) -> Result<(), S::Error> {
        if self.attached.len() >= MAX_ATTACHED {
            return Err(Error::Schema {
                kind: SchemaKind::TooManyAttached,
            });
        }
        if eq_ci(name, b"main") || eq_ci(name, b"temp") || self.att_index(name).is_some() {
            return Err(Error::Schema {
                kind: SchemaKind::DatabaseInUse,
            });
        }
        let target = if file.is_empty() {
            AttachTarget::Temporary
        } else if file == b":memory:" {
            AttachTarget::Memory
        } else {
            AttachTarget::File(file)
        };
        let storage = match self.attach_hook.as_mut() {
            Some(h) => h(target),
            None => self.pager.storage().open_attached(target),
        };
        let Some(storage) = storage else {
            return Err(Error::Schema {
                kind: SchemaKind::CannotOpenDatabase,
            });
        };
        // A file this connection already has open is refused: two
        // pagers over one file would each cache pages the other
        // changes (the pin shares one; hipp has no shared cache).
        let f = storage.file_name();
        if !f.is_empty()
            && (f == self.pager.storage().file_name()
                || self
                    .attached
                    .iter()
                    .any(|a| a.conn.pager.storage().file_name() == f))
        {
            return Err(Error::Unsupported);
        }
        let open: Boxed<'_, Result<Connection<S>, S::Error>> =
            Box::pin(Connection::open(storage, self.cfg.clone()));
        let mut c = open.await?;
        c.alias = Some(copy_vec(name));
        // The pin's rule: an attached database stores text as main
        // does; an empty one takes main's encoding.
        #[cfg(feature = "utf16")]
        {
            let enc = self.pager.text_enc();
            if c.pager.text_enc() != enc {
                if !c.pager.enc_open() {
                    return Err(Error::Schema {
                        kind: SchemaKind::AttachEncoding,
                    });
                }
                c.set_text_enc(enc);
            }
        }
        // A database attached inside a transaction joins it.
        if self.explicit_txn {
            let marks = self.pager.sp_marks();
            let mut script: AVec<AVec<u8>> = AVec::new();
            if !marks.as_slice().first().is_some_and(|m| m.1) {
                script.push(copy_vec(b"BEGIN"));
            }
            for (n, _) in marks.iter() {
                if let Some(n) = n {
                    let mut s = copy_vec(b"SAVEPOINT \"");
                    for &b in n.as_slice() {
                        s.push(b);
                        if b == b'"' {
                            s.push(b'"');
                        }
                    }
                    s.push(b'"');
                    script.push(s);
                }
            }
            for s in script.iter() {
                let r: Boxed<'_, Result<(), S::Error>> = Box::pin(run_on(&mut c, s.as_slice()));
                r.await?;
            }
        }
        self.attached.push(Attached {
            name: copy_vec(name),
            conn: Box::new(c),
            used: core::cell::Cell::new(false),
        });
        self.sync_att();
        Ok(())
    }

    /// Close every attached database (`Connection::close`).
    pub(super) async fn close_attached(&mut self) {
        while let Some(a) = self.attached.pop() {
            let close: Boxed<'_, Result<(), S::Error>> = Box::pin(a.conn.close());
            let _ = close.await;
        }
    }

    /// DETACH `name`.
    async fn detach_db(&mut self, name: &[u8]) -> Result<(), S::Error> {
        let Some(i) = self.att_index(name) else {
            let fixed = eq_ci(name, b"main") || (eq_ci(name, b"temp") && self.temp.is_some());
            return Err(Error::Schema {
                kind: if fixed {
                    SchemaKind::CannotDetach
                } else {
                    SchemaKind::NoSuchDatabase
                },
            });
        };
        if self
            .attached
            .as_slice()
            .get(i)
            .is_some_and(|a| a.used.get() || a.conn.pager.in_txn())
        {
            return Err(Error::Schema {
                kind: SchemaKind::DatabaseLocked,
            });
        }
        if let Some(a) = self.attached.remove_at(i) {
            let close: Boxed<'_, Result<(), S::Error>> = Box::pin(a.conn.close());
            let _ = close.await;
        }
        Ok(())
    }

    /// COMMIT across databases (v0.16, `attach`), before main's own:
    /// every attached database's deferred foreign keys are checked
    /// first; when more than one rollback-journal database has a
    /// write transaction and main has a file name, the pin's
    /// super-journal makes the commit atomic — named
    /// `<main>-mjXXXXXX9XX`, listing each journal (NUL-terminated),
    /// synced; each journal then gets the pointer record at its end
    /// and its database is written and synced (phase one); deleting
    /// the super-journal is the commit point; then each journal goes
    /// (phase two). Otherwise each database commits on its own (main
    /// now, the attached ones as COMMIT reaches them), as the pin's
    /// simple case does.
    pub(super) async fn commit_all(&mut self) -> Result<(), S::Error> {
        if self.attached.is_empty() {
            return Ok(());
        }
        #[cfg(feature = "foreign_keys")]
        for a in self.attached.iter() {
            a.conn.fk_commit_check()?;
        }
        let n = usize::from(self.pager.super_candidate())
            + self
                .attached
                .iter()
                .filter(|a| a.conn.pager.super_candidate())
                .count();
        let main_name = copy_vec(self.pager.storage().file_name());
        if n < 2 || main_name.is_empty() {
            return Ok(());
        }
        // The super-journal's name: main's file name, `-mj`, six hex
        // digits, `9`, two more (the pin's `sqlite3VdbeCommit`).
        let mut name = AVec::new();
        let mut file = None;
        for _ in 0..100 {
            let r = self.pager.rand_u32();
            name = main_name.clone();
            name.extend_from_slice(b"-mj");
            let hex = |v: u32, n: usize, out: &mut AVec<u8>| {
                for k in (0..n).rev() {
                    let d = ((v >> (4 * k)) & 0xf) as u8;
                    out.push(if d < 10 { b'0' + d } else { b'A' + d - 10 });
                }
            };
            hex((r >> 8) & 0xff_ffff, 6, &mut name);
            name.push(b'9');
            hex(r & 0xff, 2, &mut name);
            match self
                .pager
                .storage_mut()
                .open_named(name.as_slice())
                .await
                .map_err(Error::Storage)?
            {
                // Storage without named files: no super-journal.
                None => return Ok(()),
                Some(f) if f.existed() => continue,
                Some(f) => {
                    file = Some(f);
                    break;
                }
            }
        }
        let Some(mut file) = file else {
            return Err(Error::Unsupported);
        };
        // Each journal's name, NUL-terminated, in database order.
        let mut body = AVec::new();
        if self.pager.super_candidate() {
            body.extend_from_slice(self.pager.journal_name().as_slice());
            body.push(0);
        }
        for a in self.attached.iter() {
            if a.conn.pager.super_candidate() {
                body.extend_from_slice(a.conn.pager.journal_name().as_slice());
                body.push(0);
            }
        }
        file.write_at(0, body.as_slice())
            .await
            .map_err(Error::Storage)?;
        file.sync().await.map_err(Error::Storage)?;
        // Phase one: the pointer, the journal synced, the database
        // written and synced, the journal kept.
        if self.pager.super_candidate() {
            self.pager.commit_phase_one(name.as_slice()).await?;
        }
        for a in self.attached.as_mut_slice().iter_mut() {
            if a.conn.pager.super_candidate() {
                a.conn.pager.commit_phase_one(name.as_slice()).await?;
            }
        }
        // The commit point.
        file.delete().await.map_err(Error::Storage)?;
        // Phase two: each journal deleted. Every attached database's
        // transaction is over (its savepoints went with phase two, so
        // the COMMIT / RELEASE that reaches it next finds none).
        self.pager.drive_commit().await?;
        for a in self.attached.as_mut_slice().iter_mut() {
            a.conn.pager.drive_commit().await?;
            a.conn.pager.sp_clear();
            a.conn.explicit_txn = false;
        }
        Ok(())
    }
}

impl<S: Storage> Connection<S> {
    /// Prepare a statement routed to an attached database: prepared
    /// there now (its errors are prepare errors, as on the pin) and
    /// again when it runs.
    pub(super) async fn prepare_att(&mut self, r: OnRoute) -> Result<Plan, S::Error> {
        let OnRoute { db, sql, ext, lock } = r;
        let tabs = self.ext_tabs(ext.as_slice())?;
        self.sync_att();
        let Ok(text) = core::str::from_utf8(sql.as_slice()) else {
            return Err(Error::Unsupported);
        };
        let Some(a) = self.attached.as_mut_slice().get_mut(db) else {
            return Err(Error::Unsupported);
        };
        *a.conn.ext.borrow_mut() = tabs;
        let width = {
            let prep: Boxed<'_, Result<Statement<'_, S>, S::Error>> =
                Box::pin(a.conn.prepare(text));
            let r = prep.await;
            r.map(|st| st.column_count())
        };
        a.conn.ext.borrow_mut().clear();
        Ok(Plan::Att(Box::new(AttPlan {
            db,
            sql,
            width: width?,
            ext,
            lock,
        })))
    }
}

impl<S: Storage> Statement<'_, S> {
    /// Run a statement routed to an attached database: all of it at
    /// the first step, its rows kept to serve (as `step_temp`).
    pub(super) async fn step_att(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        if !self.query_loaded {
            let Plan::Att(p) = &self.plan else {
                return Err(Error::Unsupported);
            };
            let (db, sql, ext, lock) = (p.db, p.sql.clone(), p.ext.clone(), p.lock);
            // The tables it reads elsewhere, read now.
            let mut tabs = self.conn.ext_tabs(ext.as_slice())?;
            for t in tabs.as_mut_slice().iter_mut() {
                let rows = self.conn.ext_rows(t.db, t.name.as_slice()).await?;
                t.rows = Some(rows);
            }
            #[cfg(feature = "params")]
            let binds = self.binds.values.clone();
            self.conn.sync_att();
            let conn = &mut *self.conn;
            let (last, changes, total) = (
                conn.env.last_rowid.get(),
                conn.env.changes.get(),
                conn.env.total_changes.get(),
            );
            let in_main_txn = conn.explicit_txn;
            let Some(a) = conn.attached.as_mut_slice().get_mut(db) else {
                return Err(Error::Schema {
                    kind: SchemaKind::NoSuchDatabase,
                });
            };
            if in_main_txn && lock {
                a.used.set(true);
            }
            let c = &mut a.conn;
            let in_txn = c.explicit_txn;
            c.env.last_rowid.set(last);
            c.env.changes.set(changes);
            let before = c.env.total_changes.get();
            *c.ext.borrow_mut() = tabs;
            let Ok(text) = core::str::from_utf8(sql.as_slice()) else {
                return Err(Error::Unsupported);
            };
            let mut rows = AVec::new();
            let mut rowids = AVec::new();
            let r = async {
                let prep: Boxed<'_, Result<Statement<'_, S>, S::Error>> = Box::pin(c.prepare(text));
                let mut st = prep.await?;
                #[cfg(feature = "params")]
                for (i, v) in binds.iter().enumerate() {
                    st.bind_value(i + 1, v.as_value())?;
                }
                loop {
                    let step: Boxed<'_, Result<Step, S::Error>> = Box::pin(st.step());
                    if step.await? == Step::Done {
                        return Ok::<(), Error<S::Error>>(());
                    }
                    rows.push(st.current.clone());
                    rowids.push(st.current_rowid);
                }
            }
            .await;
            c.ext.borrow_mut().clear();
            let (l2, c2, t2) = (
                c.env.last_rowid.get(),
                c.env.changes.get(),
                c.env.total_changes.get(),
            );
            let ended = in_txn && !c.explicit_txn;
            let env = &self.conn.env;
            env.last_rowid.set(l2);
            env.changes.set(c2);
            env.total_changes
                .set(total.saturating_add(t2.saturating_sub(before)));
            // A failure that ended the attached database's transaction
            // (OR ROLLBACK, an I/O-class error) ends every database's.
            if r.is_err() && ended && self.conn.explicit_txn {
                let _ = self.conn.pager.rollback().await;
                self.conn.explicit_txn = false;
                self.conn.mirror_temp(b"ROLLBACK").await;
                self.conn.mirror_att(b"ROLLBACK").await;
            }
            r?;
            self.query_rows = rows;
            self.query_rowids = rowids;
            self.query_loaded = true;
            self.query_pos = 0;
        }
        if self.query_pos >= self.query_rows.len() {
            self.phase = Phase::Done;
            self.current_rowid = None;
            return Ok(Step::Done);
        }
        self.current = self.query_rows.as_slice()[self.query_pos].clone();
        self.current_rowid = self.query_rowids.as_slice()[self.query_pos];
        self.query_pos += 1;
        self.phase = Phase::Running;
        Ok(Step::Row)
    }

    /// Run ATTACH / DETACH.
    pub(super) async fn step_attach(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let Plan::Attach(p) = &self.plan else {
            return Err(Error::Unsupported);
        };
        let (file, name, key) = (p.file.clone(), p.name.clone(), p.key.clone());
        let empty = RowData::empty();
        let f = match &file {
            Some(f) => Some(value_text(&self.eval_row_sub(f, &empty).await?)),
            None => None,
        };
        let n = value_text(&self.eval_row_sub(&name, &empty).await?);
        if let Some(k) = &key {
            // The key is evaluated (its errors are the statement's)
            // and ignored, as by a pin built without encryption.
            let _ = self.eval_row_sub(k, &empty).await?;
        }
        match f {
            Some(f) => {
                let a: Boxed<'_, Result<(), S::Error>> =
                    Box::pin(self.conn.attach_db(f.as_slice(), n.as_slice()));
                a.await?
            }
            None => {
                let d: Boxed<'_, Result<(), S::Error>> =
                    Box::pin(self.conn.detach_db(n.as_slice()));
                d.await?
            }
        }
        self.phase = Phase::Done;
        Ok(Step::Done)
    }

    /// The rows of external source `sp.ext` (`ext_source`): the ones
    /// main handed over, or — on main — read from its database now.
    pub(super) async fn scan_ext(&mut self, sp: &super::SourcePlan) -> Result<Rows, S::Error> {
        let k = usize::from(sp.ext).saturating_sub(1);
        let (db, name) = {
            let ext = self.conn.ext.borrow();
            let Some(e) = ext.as_slice().get(k) else {
                return Err(Error::Schema {
                    kind: SchemaKind::UnknownTable,
                });
            };
            if let Some(r) = &e.rows {
                return Ok(r.clone());
            }
            (e.db, e.name.clone())
        };
        if self.conn.alias.is_some() || self.conn.is_temp {
            return Err(Error::Schema {
                kind: SchemaKind::UnknownTable,
            });
        }
        // Read once per execution (v0.16 review fix: a correlated
        // subquery read the whole table again for every outer row).
        // Nothing a statement run on main does changes another
        // database, so the rows stay the rows until `reset`.
        let rows = self.conn.ext_rows(db, name.as_slice()).await?;
        if let Some(e) = self.conn.ext.borrow_mut().as_mut_slice().get_mut(k) {
            e.rows = Some(rows.clone());
        }
        Ok(rows)
    }

    /// Forget the rows `scan_ext` kept (a new execution, after
    /// `reset`, reads them again).
    #[cfg(feature = "params")]
    pub(super) fn forget_ext_rows(&mut self) {
        if self.conn.alias.is_none() && !self.conn.is_temp {
            for e in self.conn.ext.borrow_mut().as_mut_slice().iter_mut() {
                e.rows = None;
            }
        }
    }
}
