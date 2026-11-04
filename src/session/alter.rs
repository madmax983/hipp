//! ALTER TABLE ... RENAME TO, RENAME [COLUMN] and DROP [COLUMN]
//! (v0.16, `v16`).
//!
//! The pin edits the stored `CREATE` texts token by token: every
//! token that names the altered table (or column) is replaced, the
//! rest of the text — spacing, comments, keyword case — is kept
//! byte for byte. hipp does the same, finding the tokens three ways:
//!
//! - A `CREATE TABLE` text is walked by tokens: the table's name,
//!   each column definition's name, the PRIMARY KEY / UNIQUE /
//!   FOREIGN KEY column lists, `REFERENCES parent (cols)`, and the
//!   column references inside CHECK and generated-column
//!   expressions (parsed with their tokens).
//! - A `CREATE INDEX` text likewise: the table after `ON`, and the
//!   column references of each key item and of the WHERE clause.
//! - A view is expanded and planned with an `AlterTrace` on the
//!   altered table: the planner records which name tokens resolved
//!   to it (FROM-clause names, qualifiers, columns), so scoping,
//!   aliases, CTEs and subqueries decide exactly as in a query.
//!
//! An edited token takes the new name bare when it was bare and
//! the new name was written bare; otherwise (always for RENAME TO)
//! the quoted form `"new"`, followed by a space when the next byte
//! is a `"` — the pin's `renameEditSql`, fitted to its output.
//!
//! The pin's checks: every view must plan before the change and
//! after it ("error in view v after rename"), the altered table's
//! text must still name only its own columns, and DROP COLUMN
//! refuses a PRIMARY KEY or UNIQUE column, the last column, and one
//! that an index, a CHECK, a generated column, a foreign key or a
//! view still uses. DROP COLUMN rewrites every row without the
//! column (unless it was VIRTUAL), materialising the defaults of a
//! short row and storing 0 / 1 in one byte, as the pin's rewrite
//! does.
//!
//! Triggers (v0.16, `triggers`): a trigger's text is edited as the
//! pin edits it — its `ON` table and `UPDATE OF` columns, the
//! `NEW.` / `OLD.` columns of a trigger on the altered table, the
//! targets of its INSERT / UPDATE / DELETE statements with their
//! column lists, SET targets, upsert targets and `excluded.`
//! references (RENAME TO renames the `excluded` qualifier too, as
//! the pin does), and every reference its statements resolve to
//! the table (planned as SELECTs with the trace). Every trigger must
//! plan before and after the change ("error in trigger t"). Without
//! `triggers` a file holding triggers is refused (`Unsupported`).

use super::*;
use crate::arena::ABytes;
use crate::query::AlterTrace;
use crate::sql::lexer::{Tok, Token};
use crate::sql::parser::Limits;
use alloc::rc::Rc;
use core::cell::RefCell;

/// What one ALTER TABLE does (planned).
#[derive(Debug, Clone)]
pub(super) enum AlterOp {
    /// RENAME TO `new`.
    Rename { new: AVec<u8> },
    /// RENAME COLUMN `old` (index `col`) TO `new`.
    RenameCol {
        col: usize,
        old: AVec<u8>,
        new: AVec<u8>,
        quoted: bool,
    },
    /// DROP COLUMN `name` (index `col`).
    Drop { col: usize, name: AVec<u8> },
}

/// One `sqlite_schema` row as stored.
struct Row {
    rowid: i64,
    ty: AVec<u8>,
    name: AVec<u8>,
    tbl: AVec<u8>,
    root: i64,
    sql: Option<AVec<u8>>,
}

/// One text edit: replace bytes `at..end` with `text`.
struct Ed {
    at: usize,
    end: usize,
    text: AVec<u8>,
}

fn err<E>(kind: SchemaKind) -> Error<E> {
    Error::Schema { kind }
}

fn is_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// One past the last byte of the token starting at `off`.
fn tok_end(sql: &[u8], off: usize) -> usize {
    let n = sql.len();
    let Some(&c) = sql.get(off) else {
        return off;
    };
    let close = match c {
        b'"' => b'"',
        b'`' => b'`',
        b'\'' => b'\'',
        b'[' => b']',
        _ => 0,
    };
    let mut i = off;
    if close == 0 {
        while i < n && is_id_char(sql[i]) {
            i += 1;
        }
        return i;
    }
    i += 1;
    while i < n {
        if sql[i] == close {
            if close != b']' && i + 1 < n && sql[i + 1] == close {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    n
}

/// `"name"` with every `"` doubled.
fn quoted(name: &[u8]) -> AVec<u8> {
    let mut v = AVec::new();
    v.push(b'"');
    for &b in name {
        v.push(b);
        if b == b'"' {
            v.push(b'"');
        }
    }
    v.push(b'"');
    v
}

/// The edit replacing the token at `at` with `new` (see the module
/// docs for the bare / quoted choice).
fn edit_for(sql: &[u8], at: usize, new: &[u8], force_quote: bool) -> Ed {
    let end = tok_end(sql, at);
    let bare = !force_quote && sql.get(at).is_some_and(|c| is_id_char(*c));
    let text = if bare {
        copy_vec(new)
    } else {
        let mut q = quoted(new);
        if sql.get(end) == Some(&b'"') {
            q.push(b' ');
        }
        q
    };
    Ed { at, end, text }
}

/// Apply edits (deduplicated by position) to `sql`.
fn apply(sql: &[u8], mut eds: AVec<Ed>) -> AVec<u8> {
    eds.as_mut_slice().sort_by_key(|e| e.at);
    let mut out = AVec::new();
    let mut pos = 0usize;
    for e in eds.iter() {
        if e.at < pos || e.end > sql.len() {
            continue;
        }
        out.extend_from_slice(&sql[pos..e.at]);
        out.extend_from_slice(e.text.as_slice());
        pos = e.end;
    }
    out.extend_from_slice(&sql[pos.min(sql.len())..]);
    out
}

/// The byte offset of the token whose handle is `h` (a name the
/// parser took from that token).
fn tok_offset(toks: &[Token], h: ABytes) -> Option<usize> {
    if h.len == 0 {
        return None;
    }
    toks.iter()
        .find(|t| matches!(t.tok, Tok::Ident(x, _) | Tok::Text(x) if x == h))
        .map(|t| t.offset)
}

fn tok_name<'a>(toks: &[Token], i: usize, arena: &'a Arena) -> Option<&'a [u8]> {
    match toks.get(i)?.tok {
        Tok::Ident(h, _) | Tok::Text(h) => Some(handle_bytes(arena, h)),
        _ => None,
    }
}

fn is_kw(toks: &[Token], i: usize, arena: &Arena, w: &[u8]) -> bool {
    matches!(toks.get(i).map(|t| t.tok), Some(Tok::Ident(h, false)) if eq_ci(handle_bytes(arena, h), w))
}

fn is_tok(toks: &[Token], i: usize, k: Tok) -> bool {
    toks.get(i).is_some_and(|t| t.tok == k)
}

/// The index of the `)` matching the `(` at `i`.
fn close_paren(toks: &[Token], i: usize) -> usize {
    let mut depth = 0usize;
    let mut j = i;
    while j < toks.len() {
        match toks[j].tok {
            Tok::LParen => depth += 1,
            Tok::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return j;
                }
            }
            Tok::Eof => return j,
            _ => {}
        }
        j += 1;
    }
    j
}

/// A column reference inside an expression: (qualifier token
/// offset and name, column token offset and name).
type ColRef = (Option<Named>, usize, AVec<u8>);

/// A name token: its byte offset and its (unquoted) name.
type Named = (usize, AVec<u8>);

/// A view's trace: its FROM-name offsets, and per column reference
/// (name offset, column index, qualifier offset).
type ViewTrace = (AVec<usize>, AVec<(Option<usize>, usize, Option<usize>)>);

/// A schema row to rewrite: (row index, name, tbl_name, new text).
type Change = (usize, AVec<u8>, AVec<u8>, Option<AVec<u8>>);

/// Every column reference of `e`.
fn walk_cols(e: &Expr, out: &mut AVec<(Option<ABytes>, ABytes)>) {
    use crate::sql::Expr as X;
    match e {
        X::Column { table, name } => out.push((*table, *name)),
        X::Binary(_, a, b) | X::Like(a, b, _) => {
            walk_cols(a, out);
            walk_cols(b, out);
        }
        X::Neg(a) | X::Pos(a) | X::Not(a) | X::IsNull(a, _) | X::Cast(a, _) => walk_cols(a, out),
        X::Collate(a, _) | X::BitNot(a) | X::RowItem(a, _, _) => walk_cols(a, out),
        X::InList(a, l, _) => {
            walk_cols(a, out);
            for x in l.iter() {
                walk_cols(x, out);
            }
        }
        X::Vector(l) => {
            for x in l.iter() {
                walk_cols(x, out);
            }
        }
        X::InSubquery(a, _, _) => walk_cols(a, out),
        X::Between(a, b, c, _) => {
            walk_cols(a, out);
            walk_cols(b, out);
            walk_cols(c, out);
        }
        X::Case {
            base,
            whens,
            otherwise,
        } => {
            if let Some(b) = base {
                walk_cols(b, out);
            }
            for (w, t) in whens.iter() {
                walk_cols(w, out);
                walk_cols(t, out);
            }
            if let Some(o) = otherwise {
                walk_cols(o, out);
            }
        }
        X::Func { args, clause, .. } => {
            for a in args.iter() {
                walk_cols(a, out);
            }
            if let Some(c) = clause {
                for (x, _) in c.order_by.iter() {
                    walk_cols(x, out);
                }
                if let Some(f) = &c.filter {
                    walk_cols(f, out);
                }
            }
        }
        _ => {}
    }
}

/// The column references of the expression text `sql[at..end]`.
fn expr_refs<E>(
    sql: &[u8],
    at: usize,
    end: usize,
    arena: &Arena,
    lim: Limits,
) -> Result<AVec<ColRef>, E> {
    let sub = sql.get(at..end).unwrap_or(&[]);
    let (e, toks) = crate::sql::parser::parse_expr_with_tokens::<E>(sub, arena, lim)?;
    let mut cols = AVec::new();
    walk_cols(&e, &mut cols);
    let mut out = AVec::new();
    for (q, n) in cols.iter() {
        let Some(no) = tok_offset(toks.as_slice(), *n) else {
            continue;
        };
        let qual = q.and_then(|q| {
            tok_offset(toks.as_slice(), q).map(|o| (o + at, copy_vec(handle_bytes(arena, q))))
        });
        out.push((qual, no + at, copy_vec(handle_bytes(arena, *n))));
    }
    Ok(out)
}

/// A CREATE TABLE text, walked by tokens.
#[derive(Default)]
struct TableText {
    /// Byte offset of the table's name.
    name: usize,
    /// Each column definition: its name's offset and name, its
    /// first and last token, whether it has PRIMARY KEY / UNIQUE.
    cols: AVec<(usize, AVec<u8>, bool)>,
    /// Token index of each item's first token, and of the `)`.
    items: AVec<usize>,
    /// Byte offset of the top-level `,` token before each item
    /// (the first item's is the `(`).
    commas: AVec<usize>,
    col_items: AVec<usize>,
    close: usize,
    /// PRIMARY KEY / UNIQUE lists (table constraints): offsets and
    /// names.
    keys: AVec<(usize, AVec<u8>)>,
    /// FOREIGN KEY (...) child columns.
    fk_local: AVec<(usize, AVec<u8>)>,
    /// REFERENCES parent [(cols)]: parent's offset and name, then
    /// its columns.
    refs: AVec<(Named, AVec<Named>)>,
    /// Column references in CHECK / generated expressions, with
    /// the item they belong to.
    exprs: AVec<(usize, ColRef)>,
}

/// The names of a `( name [COLLATE x] [ASC|DESC] [AUTOINCREMENT],
/// ... )` list whose `(` is token `i`.
fn name_list(toks: &[Token], i: usize, arena: &Arena) -> (AVec<(usize, AVec<u8>)>, usize) {
    let close = close_paren(toks, i);
    let mut out = AVec::new();
    let mut want = true;
    let mut j = i + 1;
    while j < close {
        match toks[j].tok {
            Tok::Comma => want = true,
            Tok::Ident(..) | Tok::Text(_) if want => {
                if let Some(n) = tok_name(toks, j, arena) {
                    out.push((toks[j].offset, copy_vec(n)));
                }
                want = false;
            }
            Tok::LParen => j = close_paren(toks, j),
            _ => {}
        }
        j += 1;
    }
    (out, close)
}

fn walk_table<E>(sql: &[u8], arena: &Arena, lim: Limits) -> Result<TableText, E> {
    let toks = crate::sql::parser::tokens::<E>(sql, arena)?;
    let t = toks.as_slice();
    let mut w = TableText::default();
    let Some(open) = t.iter().position(|x| x.tok == Tok::LParen) else {
        return Err(Error::Unsupported);
    };
    w.name = t.get(open.wrapping_sub(1)).map_or(0, |x| x.offset);
    w.close = close_paren(t, open);
    // Items: split at the top-level commas.
    let mut starts = AVec::new();
    starts.push(open + 1);
    let mut j = open + 1;
    while j < w.close {
        match t[j].tok {
            Tok::LParen => j = close_paren(t, j),
            Tok::Comma => starts.push(j + 1),
            _ => {}
        }
        j += 1;
    }
    for (k, &from) in starts.iter().enumerate() {
        let to = starts.as_slice().get(k + 1).map_or(w.close, |n| n - 1);
        w.items.push(from);
        let first_kw = |i: usize| {
            ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"]
                .iter()
                .any(|k| is_kw(t, i, arena, k.as_bytes()))
        };
        let is_col = !first_kw(from);
        let mut j = from;
        let mut pk_uniq = false;
        if is_col {
            w.col_items.push(k);
            j += 1;
        }
        while j < to {
            if is_kw(t, j, arena, b"CONSTRAINT") || is_kw(t, j, arena, b"COLLATE") {
                j += 2;
                continue;
            }
            let paren = is_tok(t, j + 1, Tok::LParen);
            if paren && (is_kw(t, j, arena, b"CHECK") || is_kw(t, j, arena, b"AS")) {
                let close = close_paren(t, j + 1);
                let at = t.get(j + 2).map_or(0, |x| x.offset);
                let end = t.get(close).map_or(sql.len(), |x| x.offset);
                for r in expr_refs::<E>(sql, at, end, arena, lim)?.iter() {
                    w.exprs.push((k, r.clone()));
                }
                j = close + 1;
                continue;
            }
            if paren && is_kw(t, j, arena, b"DEFAULT") {
                j = close_paren(t, j + 1) + 1;
                continue;
            }
            if paren && (is_kw(t, j, arena, b"KEY") || is_kw(t, j, arena, b"UNIQUE")) {
                let fk = j > 0 && is_kw(t, j - 1, arena, b"FOREIGN");
                let (names, close) = name_list(t, j + 1, arena);
                for n in names.iter() {
                    if fk {
                        w.fk_local.push(n.clone());
                    } else {
                        w.keys.push(n.clone());
                    }
                }
                j = close + 1;
                continue;
            }
            if is_kw(t, j, arena, b"PRIMARY") || is_kw(t, j, arena, b"UNIQUE") {
                pk_uniq = true;
            }
            if is_kw(t, j, arena, b"REFERENCES") {
                let parent = match tok_name(t, j + 1, arena) {
                    Some(n) => (t[j + 1].offset, copy_vec(n)),
                    None => (0, AVec::new()),
                };
                let mut cols = AVec::new();
                j += 2;
                if is_tok(t, j, Tok::LParen) {
                    let (names, close) = name_list(t, j, arena);
                    cols = names;
                    j = close + 1;
                }
                w.refs.push((parent, cols));
                continue;
            }
            if is_tok(t, j, Tok::LParen) {
                j = close_paren(t, j);
            }
            j += 1;
        }
        if is_col {
            let name = tok_name(t, from, arena).map(copy_vec).unwrap_or_default();
            w.cols
                .push((t.get(from).map_or(0, |x| x.offset), name, pk_uniq));
        }
    }
    // Byte offsets of the item starts and the `)`.
    let mut item_offs = AVec::new();
    for &i in w.items.iter() {
        item_offs.push(t.get(i).map_or(0, |x| x.offset));
        w.commas
            .push(t.get(i.wrapping_sub(1)).map_or(0, |x| x.offset));
    }
    w.items = item_offs;
    w.close = t.get(w.close).map_or(sql.len(), |x| x.offset);
    Ok(w)
}

/// A CREATE INDEX text, walked by tokens: the table name's offset
/// and name, and the column references of its key items and WHERE.
fn walk_index<E>(sql: &[u8], arena: &Arena, lim: Limits) -> Result<(Named, AVec<ColRef>), E> {
    let toks = crate::sql::parser::tokens::<E>(sql, arena)?;
    let t = toks.as_slice();
    let Some(on) = (0..t.len()).find(|&i| is_kw(t, i, arena, b"ON")) else {
        return Err(Error::Unsupported);
    };
    let table = (
        t.get(on + 1).map_or(0, |x| x.offset),
        tok_name(t, on + 1, arena).map(copy_vec).unwrap_or_default(),
    );
    let mut refs = AVec::new();
    let open = on + 2;
    if !is_tok(t, open, Tok::LParen) {
        return Err(Error::Unsupported);
    }
    let close = close_paren(t, open);
    let mut from = open + 1;
    let mut j = open + 1;
    while j <= close {
        if j == close || is_tok(t, j, Tok::Comma) {
            // The item is tokens from..j, less a trailing ASC / DESC.
            let mut last = j;
            if last > from
                && (is_kw(t, last - 1, arena, b"ASC") || is_kw(t, last - 1, arena, b"DESC"))
            {
                last -= 1;
            }
            if last > from {
                let at = t[from].offset;
                let end = t.get(last).map_or(sql.len(), |x| x.offset);
                for r in expr_refs::<E>(sql, at, end, arena, lim)?.iter() {
                    refs.push(r.clone());
                }
            }
            from = j + 1;
        } else if is_tok(t, j, Tok::LParen) {
            j = close_paren(t, j);
            continue;
        }
        j += 1;
    }
    if is_kw(t, close + 1, arena, b"WHERE") {
        let at = t.get(close + 2).map_or(sql.len(), |x| x.offset);
        let mut end = sql.len();
        while end > at && (sql[end - 1] == b';' || sql[end - 1].is_ascii_whitespace()) {
            end -= 1;
        }
        for r in expr_refs::<E>(sql, at, end, arena, lim)?.iter() {
            refs.push(r.clone());
        }
    }
    Ok((table, refs))
}

/// Whether a column reference of an expression in `table`'s own
/// text (or its index) names column `col`.
fn names_col(r: &ColRef, table: &[u8], col: &[u8]) -> bool {
    r.0.as_ref().is_none_or(|(_, q)| eq_ci(q.as_slice(), table)) && eq_ci(r.2.as_slice(), col)
}

impl<S: Storage> Connection<S> {
    /// Plan ALTER TABLE RENAME TO / RENAME COLUMN / DROP COLUMN
    /// (v0.16, `v16`): the checks the pin makes when it prepares
    /// the statement.
    pub(super) fn plan_alter16(&self, arena: &Arena, ast: &Ast) -> Result<Plan, S::Error> {
        let table = match ast {
            Ast::AlterRename { table, .. }
            | Ast::AlterRenameColumn { table, .. }
            | Ast::AlterDropColumn { table, .. } => handle_bytes(arena, *table),
            _ => return Err(Error::Unsupported),
        };
        let Some(t) = self.catalog.find(&self.catalog_arena, table) else {
            return Err(if Catalog::is_schema_name(table) {
                err(SchemaKind::SystemTable)
            } else if self.catalog.find_view(&self.catalog_arena, table).is_some() {
                err(SchemaKind::IsView)
            } else {
                err(SchemaKind::UnknownTable)
            });
        };
        if table.get(..7).is_some_and(|p| eq_ci(p, b"sqlite_")) {
            return Err(err(SchemaKind::SystemTable));
        }
        let t = t.usable()?;
        let ca = &self.catalog_arena;
        let col_of = |n: &[u8]| {
            t.columns
                .iter()
                .position(|c| eq_ci(handle_bytes(ca, c.name), n))
                .ok_or(err::<S::Error>(SchemaKind::UnknownColumn))
        };
        let op = match ast {
            Ast::AlterRename { to, .. } => {
                let new = handle_bytes(arena, *to);
                reserved_name::<S::Error>(new)?;
                if self.catalog.taken(ca, new).is_some() {
                    return Err(err(SchemaKind::DuplicateTable));
                }
                AlterOp::Rename { new: copy_vec(new) }
            }
            Ast::AlterRenameColumn {
                from, to, quoted, ..
            } => {
                let col = col_of(handle_bytes(arena, *from))?;
                let new = handle_bytes(arena, *to);
                if crate::query::is_rowid_name(new) {
                    return Err(err(SchemaKind::RowidAliasName));
                }
                if t.columns
                    .iter()
                    .enumerate()
                    .any(|(j, c)| j != col && eq_ci(handle_bytes(ca, c.name), new))
                {
                    return Err(err(SchemaKind::DuplicateColumn));
                }
                AlterOp::RenameCol {
                    col,
                    old: copy_vec(handle_bytes(ca, t.columns.as_slice()[col].name)),
                    new: copy_vec(new),
                    quoted: *quoted,
                }
            }
            Ast::AlterDropColumn { column, .. } => {
                let col = col_of(handle_bytes(arena, *column))?;
                if t.columns.len() < 2 || t.rowid_alias == Some(col) {
                    return Err(err(SchemaKind::CannotDropColumn));
                }
                AlterOp::Drop {
                    col,
                    name: copy_vec(handle_bytes(ca, t.columns.as_slice()[col].name)),
                }
            }
            _ => return Err(Error::Unsupported),
        };
        Ok(Plan::Alter16 {
            table: copy_vec(handle_bytes(ca, t.name)),
            op,
        })
    }

    /// Plan a view's stored text `sql` against the current schema
    /// (the TEMP database's second RENAME COLUMN pass, above).
    fn plan_view_text(&self, arena: &Arena, name: &[u8], sql: &[u8]) -> Result<(), S::Error> {
        let Ast::CreateView { select, .. } =
            crate::sql::parse::<S::Error>(sql, arena, self.cfg.limits())?
        else {
            return Err(Error::Unsupported);
        };
        let expanded = self.expand_parsed_view(arena, name, select)?;
        self.plan_subquery(&expanded, arena, &[])?;
        Ok(())
    }

    /// A view's SELECT planned with a trace on `table` (see the
    /// module docs): the trace's FROM names and column references,
    /// as byte offsets into `sql`. The view's unused CTEs are
    /// planned too (the pin edits their bodies as well).
    fn trace_view(
        &mut self,
        arena: &Arena,
        name: &[u8],
        sql: &[u8],
        table: &[u8],
    ) -> Result<ViewTrace, S::Error> {
        let (ast, toks) =
            crate::sql::parser::parse_with_tokens::<S::Error>(sql, arena, self.cfg.limits())?;
        let Ast::CreateView { select, .. } = ast else {
            return Err(Error::Unsupported);
        };
        let tr = Rc::new(RefCell::new(AlterTrace {
            table: copy_vec(table),
            ..AlterTrace::default()
        }));
        self.alter_trace = Some(tr.clone());
        let r = (|| -> Result<(), S::Error> {
            let expanded = self.expand_parsed_view(arena, name, select.clone())?;
            self.plan_subquery(&expanded, arena, &[])?;
            // Each CTE of the view's WITH clause, read on its own.
            if let Some(w) = &select.with {
                for c in w.ctes.iter() {
                    let mut probe = select.clone();
                    probe.items = AVec::new();
                    probe.items.push(SelectItem::Star);
                    probe.from = AVec::new();
                    probe.from.push(crate::sql::FromSource {
                        table: arena.alloc_copy_handle(handle_bytes(arena, c.name))?,
                        alias: None,
                        kind: crate::sql::JoinKind::Inner,
                        on: None,
                        derived: None,
                        natural: false,
                        using: AVec::new(),
                        args: None,
                        qualified: false,
                        cte: None,
                    });
                    probe.filter = None;
                    probe.group_by = AVec::new();
                    probe.having = None;
                    probe.order_by = AVec::new();
                    probe.limit = None;
                    probe.offset = None;
                    probe.compound = AVec::new();
                    probe.values = AVec::new();
                    if let Ok(e) = self.expand_parsed_view(arena, name, probe) {
                        let _ = self.plan_subquery(&e, arena, &[]);
                    }
                }
            }
            Ok(())
        })();
        self.alter_trace = None;
        r?;
        // A compound's ORDER BY term naming a result column reads
        // that column (the pin renames the term with it).
        {
            let mut trm = tr.borrow_mut();
            compound_order_hits(&select, arena, &mut trm.hits);
            if let Some(w) = &select.with {
                for c in w.ctes.iter() {
                    compound_order_hits(&c.select, arena, &mut trm.hits);
                }
            }
        }
        let tr = tr.borrow();
        let t = toks.as_slice();
        let at = |pos: u32| {
            t.iter()
                .find(|x| matches!(x.tok, Tok::Ident(h, _) | Tok::Text(h) if h.pos == pos && h.len > 0))
                .map(|x| x.offset)
        };
        let mut from = AVec::new();
        for &p in tr.from.iter() {
            if let Some(o) = at(p) {
                from.push(o);
            }
        }
        let mut hits = AVec::new();
        for h in tr.hits.iter() {
            hits.push((h.name.and_then(at), h.col, h.qual.and_then(at)));
        }
        Ok((from, hits))
    }
}

#[cfg(feature = "triggers")]
impl<S: Storage> Connection<S> {
    /// Every trigger must still read as the pin reads it after an
    /// ALTER TABLE (`trace_trigger` with no table to edit).
    fn check_triggers(&mut self, arena: &Arena) -> Result<(), S::Error> {
        let ca = &self.catalog_arena;
        let mut texts: AVec<AVec<u8>> = AVec::new();
        for t in self.catalog.triggers.iter() {
            texts.push(copy_vec(handle_bytes(ca, t.sql)));
        }
        for t in texts.iter() {
            let ckpt = arena.checkpoint();
            let r = self.trace_trigger(
                arena,
                t.as_slice(),
                &[],
                &AlterOp::Drop {
                    col: usize::MAX,
                    name: AVec::new(),
                },
            );
            arena.rewind(ckpt);
            r?;
        }
        Ok(())
    }

    /// The edits of one trigger's text for `op` on `table` (see the
    /// module docs).
    fn trace_trigger(
        &mut self,
        arena: &Arena,
        sql: &[u8],
        table: &[u8],
        op: &AlterOp,
    ) -> Result<AVec<Ed>, S::Error> {
        let lim = self.cfg.limits();
        let (ast, toks) = crate::sql::parser::parse_with_tokens::<S::Error>(sql, arena, lim)?;
        let Ast::CreateTrigger(ct) = ast else {
            return Err(Error::Unsupported);
        };
        let mut eds = AVec::new();
        let on = handle_bytes(arena, ct.table);
        let on_it = eq_ci(on, table);
        let tk = toks.as_slice();
        if on_it {
            match op {
                AlterOp::Rename { new } => {
                    if let Some(o) = tok_offset(tk, ct.table) {
                        eds.push(edit_for(sql, o, new.as_slice(), true));
                    }
                }
                AlterOp::RenameCol {
                    old, new, quoted, ..
                } => {
                    for c in ct.of_cols.iter() {
                        if eq_ci(handle_bytes(arena, *c), old.as_slice())
                            && let Some(o) = tok_offset(tk, *c)
                        {
                            eds.push(edit_for(sql, o, new.as_slice(), *quoted));
                        }
                    }
                }
                AlterOp::Drop { .. } => {}
            }
        }
        // The NEW / OLD rows bind to NULLs for planning.
        let Ok((names, colls, alias, view)) = self.trigger_columns_of(on, arena) else {
            return Ok(eds);
        };
        let mut nulls = AVec::new();
        for _ in names.iter() {
            nulls.push(OwnedValue::Null);
        }
        let row = crate::sql::trig::RowRef {
            vals: nulls.as_slice(),
            rowid: 0,
        };
        let bind = crate::sql::trig::Binding {
            names: names.as_slice(),
            colls: colls.as_slice(),
            alias,
            view,
            new: (ct.event != crate::sql::ast::TrigEvent::Delete).then_some(row),
            old: (ct.event != crate::sql::ast::TrigEvent::Insert).then_some(row),
        };
        let mut pieces: AVec<(usize, usize, usize)> = AVec::new();
        if let Some((a, b)) = ct.when {
            pieces.push((a, b, 7));
        }
        for (a, b) in ct.steps.iter() {
            pieces.push((*a, *b, 0));
        }
        for (a, b, pre) in pieces.iter().copied() {
            let mut text = AVec::new();
            if pre > 0 {
                text.extend_from_slice(b"SELECT ");
            }
            text.extend_from_slice(sql.get(a..b).unwrap_or(&[]));
            let (step, stoks) = crate::sql::parser::parse_trigger_step_tokens::<S::Error>(
                text.as_slice(),
                arena,
                lim,
            )?;
            let st = stoks.as_slice();
            let off = |h: ABytes| -> Option<usize> {
                tok_offset(st, h)
                    .and_then(|o| o.checked_sub(pre))
                    .and_then(|o| o.checked_add(a))
            };
            // NEW. / OLD. columns of a trigger on the table.
            if on_it
                && let AlterOp::RenameCol {
                    col, new, quoted, ..
                } = op
            {
                for (_, n) in crate::sql::trig::trigger_refs::<S::Error>(&step, arena)?.iter() {
                    let nb = handle_bytes(arena, *n);
                    let hit = names.iter().position(|c| eq_ci(c.as_slice(), nb)) == Some(*col);
                    if hit && let Some(o) = off(*n) {
                        eds.push(edit_for(sql, o, new.as_slice(), *quoted));
                    }
                }
            }
            target_edits(&step, arena, table, op, &off, sql, &mut eds);
            // An INSERT's target must exist (the pin resolves it).
            if let Ast::Insert { table: t, .. } = &step {
                let n = handle_bytes(arena, *t);
                if self.catalog.find(&self.catalog_arena, n).is_none()
                    && self.catalog.find_view(&self.catalog_arena, n).is_none()
                {
                    return Err(err(SchemaKind::UnknownTable));
                }
            }
            // Every reference the statement resolves to the table.
            let mut bound = step.clone();
            crate::sql::trig::bind_trigger::<S::Error>(&mut bound, &bind, arena)?;
            let Some(sel) = as_select(&bound, arena) else {
                continue;
            };
            let tr = Rc::new(RefCell::new(AlterTrace {
                table: copy_vec(table),
                ..AlterTrace::default()
            }));
            self.alter_trace = Some(tr.clone());
            let r = (|| -> Result<(), S::Error> {
                let wrapped = Ast::Select(sel);
                let expanded = self.expand_ast(arena, &wrapped)?;
                if let Some(Ast::Select(s)) = expanded.as_ref().or(Some(&wrapped)) {
                    self.plan_subquery(s, arena, &[])?;
                }
                Ok(())
            })();
            self.alter_trace = None;
            r?;
            let tr = tr.borrow();
            let at = |pos: u32| {
                st.iter()
                    .find(|x| matches!(x.tok, Tok::Ident(h, _) | Tok::Text(h) if h.pos == pos && h.len > 0))
                    .and_then(|x| x.offset.checked_sub(pre))
                    .and_then(|o| o.checked_add(a))
            };
            match op {
                AlterOp::Rename { new } => {
                    for &p in tr.from.iter() {
                        if let Some(o) = at(p) {
                            eds.push(edit_for(sql, o, new.as_slice(), true));
                        }
                    }
                    for h in tr.hits.iter() {
                        if let Some(o) = h.qual.and_then(at) {
                            eds.push(edit_for(sql, o, new.as_slice(), true));
                        }
                    }
                }
                AlterOp::RenameCol {
                    col, new, quoted, ..
                } => {
                    for h in tr.hits.iter() {
                        if h.col == *col
                            && let Some(o) = h.name.and_then(at)
                        {
                            eds.push(edit_for(sql, o, new.as_slice(), *quoted));
                        }
                    }
                }
                AlterOp::Drop { .. } => {}
            }
        }
        Ok(eds)
    }

    /// `trigger_columns` for a trigger's table or view.
    #[allow(clippy::type_complexity)]
    fn trigger_columns_of(
        &self,
        table: &[u8],
        arena: &Arena,
    ) -> Result<
        (
            AVec<AVec<u8>>,
            AVec<Option<crate::record::Coll>>,
            Option<usize>,
            bool,
        ),
        S::Error,
    > {
        let ca = &self.catalog_arena;
        let mut names = AVec::new();
        let mut colls = AVec::new();
        if let Some(t) = self.catalog.find(ca, table) {
            for c in t.columns.iter() {
                names.push(copy_vec(handle_bytes(ca, c.name)));
                colls.push(Some(c.coll));
            }
            return Ok((names, colls, t.rowid_alias, false));
        }
        let Some(v) = self.catalog.find_view(ca, table) else {
            return Err(err(SchemaKind::UnknownTable));
        };
        for n in self.view_names(arena, v)?.iter() {
            names.push(n.clone());
            colls.push(None);
        }
        Ok((names, colls, None, true))
    }
}

/// A trigger statement as the SELECT that reads what it reads: an
/// UPDATE's SET values over its target and FROM sources, a DELETE's
/// target, an INSERT's rows (`None`: nothing to plan).
#[cfg(feature = "triggers")]
fn as_select(step: &Ast, arena: &Arena) -> Option<SelectStmt> {
    match step {
        Ast::Select(s) => Some(s.clone()),
        Ast::Insert {
            select: Some(s), ..
        } => Some(s.as_ref().clone()),
        Ast::Insert { table, rows, .. } => {
            let mut s = crate::sql::parser::star_select(*table, None, None, AVec::new(), arena);
            s.items.clear();
            s.from.clear();
            s.values = rows.clone();
            Some(s)
        }
        Ast::Update {
            table,
            alias,
            assignments,
            from,
            filter,
            ..
        } => {
            let mut extra = AVec::new();
            for (_, e) in assignments.iter() {
                extra.push(e.clone());
            }
            let mut s =
                crate::sql::parser::star_select(*table, *alias, filter.clone(), extra, arena);
            let mut items = AVec::new();
            for it in s.items.iter().skip(1) {
                items.push(it.clone());
            }
            s.items = items;
            for f in from.iter() {
                s.from.push(f.clone());
            }
            Some(s)
        }
        Ast::Delete { table, filter, .. } => Some(crate::sql::parser::star_select(
            *table,
            None,
            filter.clone(),
            AVec::new(),
            arena,
        )),
        _ => None,
    }
}

/// A trigger statement's edits that name resolution does not find:
/// its INSERT target, the column lists and SET targets of a write of
/// the table, and an upsert's references.
#[cfg(feature = "triggers")]
fn target_edits(
    step: &Ast,
    arena: &Arena,
    table: &[u8],
    op: &AlterOp,
    off: &dyn Fn(ABytes) -> Option<usize>,
    sql: &[u8],
    eds: &mut AVec<Ed>,
) {
    let is_t = |h: ABytes| eq_ci(handle_bytes(arena, h), table);
    let mut push = |h: ABytes, new: &[u8], q: bool| {
        if let Some(o) = off(h) {
            eds.push(edit_for(sql, o, new, q));
        }
    };
    let (target, cols, sets, upsert) = match step {
        Ast::Insert {
            table: t,
            columns,
            upsert,
            ..
        } => (*t, columns.as_ref(), None, upsert.as_deref()),
        Ast::Update {
            table: t,
            assignments,
            ..
        } => (*t, None, Some(assignments), None),
        _ => return,
    };
    if !is_t(target) {
        return;
    }
    match op {
        AlterOp::Rename { new } => {
            if matches!(step, Ast::Insert { .. }) {
                push(target, new.as_slice(), true);
            }
            if let Some(u) = upsert {
                // The pin renames the `excluded` qualifier too.
                let mut refs = AVec::new();
                upsert_refs(u, &mut refs);
                for (q, _) in refs.iter() {
                    if let Some(q) = q
                        && (is_t(*q) || eq_ci(handle_bytes(arena, *q), b"excluded"))
                    {
                        push(*q, new.as_slice(), true);
                    }
                }
            }
        }
        AlterOp::RenameCol {
            old, new, quoted, ..
        } => {
            let is_old = |h: ABytes| eq_ci(handle_bytes(arena, h), old.as_slice());
            for c in cols.into_iter().flat_map(|c| c.iter()) {
                if is_old(*c) {
                    push(*c, new.as_slice(), *quoted);
                }
            }
            for (c, _) in sets.into_iter().flat_map(|s| s.iter()) {
                if is_old(*c) {
                    push(*c, new.as_slice(), *quoted);
                }
            }
            if let Some(u) = upsert {
                for c in u.target.iter() {
                    if is_old(*c) {
                        push(*c, new.as_slice(), *quoted);
                    }
                }
                if let crate::sql::UpsertAction::Update { assignments, .. } = &u.action {
                    for (c, _) in assignments.iter() {
                        if is_old(*c) {
                            push(*c, new.as_slice(), *quoted);
                        }
                    }
                }
                let mut refs = AVec::new();
                upsert_refs(u, &mut refs);
                for (q, n) in refs.iter() {
                    let own =
                        q.is_none_or(|q| is_t(q) || eq_ci(handle_bytes(arena, q), b"excluded"));
                    if own && is_old(*n) {
                        push(*n, new.as_slice(), *quoted);
                    }
                }
            }
        }
        AlterOp::Drop { .. } => {}
    }
}

/// The column references of an upsert's DO UPDATE clause.
#[cfg(feature = "triggers")]
fn upsert_refs(u: &crate::sql::Upsert, out: &mut AVec<(Option<ABytes>, ABytes)>) {
    if let crate::sql::UpsertAction::Update {
        assignments,
        filter,
    } = &u.action
    {
        for (_, e) in assignments.iter() {
            walk_cols(e, out);
        }
        if let Some(f) = filter {
            walk_cols(f, out);
        }
    }
}

impl<'c, S: Storage> Statement<'c, S> {
    pub(super) async fn step_alter16(&mut self) -> Result<Step, S::Error> {
        if self.phase == Phase::Done {
            return Ok(Step::Done);
        }
        let r = self.exec_alter16().await;
        match r {
            Ok(()) => {
                self.phase = Phase::Done;
                Ok(Step::Done)
            }
            Err(e) => {
                abort_write!(self, &e);
                // The catalog may have been rebuilt (or half-built)
                // from the edited schema: read it back from the
                // restored file.
                let _ = self.conn.rebuild_catalog().await;
                Err(e)
            }
        }
    }

    async fn alter_schema_rows(&mut self) -> Result<AVec<Row>, S::Error> {
        let mut rows = AVec::new();
        let mut cursor = ScanCursor::new(1);
        let mut buf = AVec::new();
        while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
            let v = crate::record::decode_record::<S::Error>(buf.as_slice(), 5)?;
            let text = |i: usize| match v.as_slice().get(i) {
                Some(Value::Text(t)) => copy_vec(t),
                _ => AVec::new(),
            };
            rows.push(Row {
                rowid,
                ty: text(0),
                name: text(1),
                tbl: text(2),
                root: match v.as_slice().get(3) {
                    Some(Value::Integer(i)) => *i,
                    _ => 0,
                },
                sql: match v.as_slice().get(4) {
                    Some(Value::Text(t)) => Some(copy_vec(t)),
                    _ => None,
                },
            });
        }
        Ok(rows)
    }

    async fn exec_alter16(&mut self) -> Result<(), S::Error> {
        let Plan::Alter16 { table, op } = &self.plan else {
            return Err(Error::Unsupported);
        };
        let (table, op) = (table.clone(), op.clone());
        let tn = table.as_slice();
        let rows = self.alter_schema_rows().await?;
        #[cfg(not(feature = "triggers"))]
        if rows.iter().any(|r| eq_ci(r.ty.as_slice(), b"trigger")) {
            return Err(Error::Unsupported);
        }
        let arena = Arena::new(
            Scope::Statement,
            self.conn.cfg.stmt_init,
            self.conn.cfg.stmt_max,
        );
        // TEMP views reading this main table (`temp_views_read`).
        if self.conn.temp_views_read(tn, &arena) {
            return Err(Error::Unsupported);
        }
        // The dropped column's record layout, read before the schema
        // changes: (stored, default) per column, and its position.
        let mut layout: AVec<(bool, OwnedValue)> = AVec::new();
        let mut troot = 0u32;
        if let Some(t) = self.conn.catalog.find(&self.conn.catalog_arena, tn) {
            troot = t.root;
            for (i, c) in t.columns.iter().enumerate() {
                let stored = c.gen_expr.is_none() || c.stored;
                let d = if Some(i) == t.rowid_alias {
                    OwnedValue::Null
                } else {
                    read_default(&self.conn.catalog_arena, c)
                };
                layout.push((stored, d));
            }
        }
        // Phase 1: the new texts (and the pin's checks on them).
        let mut changed: AVec<Change> = AVec::new();
        for (i, row) in rows.iter().enumerate() {
            let Some(sql) = &row.sql else {
                // An automatic index: renamed with its table.
                if let AlterOp::Rename { new } = &op
                    && eq_ci(row.ty.as_slice(), b"index")
                    && eq_ci(row.tbl.as_slice(), tn)
                {
                    changed.push((
                        i,
                        auto_index_name(row.name.as_slice(), tn, new.as_slice()),
                        new.clone(),
                        None,
                    ));
                }
                continue;
            };
            let sql = sql.as_slice();
            let ckpt = arena.checkpoint();
            let new_sql = if eq_ci(row.ty.as_slice(), b"table") {
                let own = eq_ci(row.name.as_slice(), tn);
                Some(self.edit_table(sql, own, tn, &op, &arena)?)
            } else if eq_ci(row.ty.as_slice(), b"index") && eq_ci(row.tbl.as_slice(), tn) {
                Some(edit_index::<S::Error>(
                    sql,
                    tn,
                    &op,
                    &arena,
                    self.conn.cfg.limits(),
                )?)
            } else if cfg!(feature = "triggers") && eq_ci(row.ty.as_slice(), b"trigger") {
                #[cfg(feature = "triggers")]
                {
                    let eds = self.conn.trace_trigger(&arena, sql, tn, &op)?;
                    if eds.is_empty() {
                        None
                    } else {
                        Some(apply(sql, eds))
                    }
                }
                #[cfg(not(feature = "triggers"))]
                None
            } else if eq_ci(row.ty.as_slice(), b"view") {
                let (from, hits) = self.conn.trace_view(&arena, row.name.as_slice(), sql, tn)?;
                let mut eds = AVec::new();
                match &op {
                    AlterOp::Rename { new } => {
                        for &o in from.iter() {
                            eds.push(edit_for(sql, o, new.as_slice(), true));
                        }
                        for (_, _, q) in hits.iter() {
                            if let Some(o) = q {
                                eds.push(edit_for(sql, *o, new.as_slice(), true));
                            }
                        }
                    }
                    AlterOp::RenameCol {
                        col, new, quoted, ..
                    } => {
                        for (n, c, _) in hits.iter() {
                            if let (Some(o), true) = (n, c == col) {
                                eds.push(edit_for(sql, *o, new.as_slice(), *quoted));
                            }
                        }
                    }
                    AlterOp::Drop { .. } => {}
                }
                if eds.is_empty() {
                    None
                } else {
                    let out = apply(sql, eds);
                    // The pin edits a TEMP view twice when the column
                    // is a TEMP table's: its second pass reads the
                    // already-edited text against the old columns,
                    // and fails when that no longer plans ("error in
                    // view v: no such column: z") — unless every
                    // edited name is a bare `"new"`, which then reads
                    // as a string there (the double-quoted-string
                    // fallback hipp's planner does not have).
                    let lenient = match &op {
                        AlterOp::RenameCol { col, quoted, .. } => {
                            *quoted
                                && !hits
                                    .iter()
                                    .any(|(n, c, q)| n.is_some() && c == col && q.is_some())
                        }
                        _ => true,
                    };
                    if self.conn.is_temp && !lenient {
                        self.conn
                            .plan_view_text(&arena, row.name.as_slice(), out.as_slice())?;
                    }
                    Some(out)
                }
            } else {
                None
            };
            arena.rewind(ckpt);
            let (name, tbl) = match &op {
                AlterOp::Rename { new } if eq_ci(row.tbl.as_slice(), tn) => {
                    let name = if eq_ci(row.ty.as_slice(), b"table") {
                        new.clone()
                    } else if eq_ci(row.ty.as_slice(), b"trigger") {
                        row.name.clone()
                    } else {
                        auto_index_name(row.name.as_slice(), tn, new.as_slice())
                    };
                    (name, new.clone())
                }
                _ => (row.name.clone(), row.tbl.clone()),
            };
            let same = new_sql.as_ref().is_none_or(|s| s.as_slice() == sql)
                && name.as_slice() == row.name.as_slice()
                && tbl.as_slice() == row.tbl.as_slice();
            if !same {
                changed.push((i, name, tbl, Some(new_sql.unwrap_or_else(|| copy_vec(sql)))));
            }
        }
        // Phase 2: write the changed rows in place.
        for (i, name, tbl, sql) in changed.iter() {
            let row = &rows.as_slice()[*i];
            let sql = match sql {
                Some(s) => Some(s.as_slice()),
                None => row.sql.as_ref().map(|s| s.as_slice()),
            };
            let mut vals: AVec<Value<'_>> = AVec::new();
            vals.push(Value::Text(row.ty.as_slice()));
            vals.push(Value::Text(name.as_slice()));
            vals.push(Value::Text(tbl.as_slice()));
            vals.push(Value::Integer(row.root));
            vals.push(match sql {
                Some(s) => Value::Text(s),
                None => Value::Null,
            });
            let mut rec = AVec::new();
            encode_record(vals.as_slice(), &mut rec);
            btree::btree_delete(&mut self.conn.pager, 1, row.rowid).await?;
            btree::btree_insert(&mut self.conn.pager, 1, row.rowid, rec.as_slice()).await?;
        }
        // The AUTOINCREMENT counter follows the table's name.
        if let AlterOp::Rename { new } = &op
            && let Some(sq) = self
                .conn
                .catalog
                .find(&self.conn.catalog_arena, b"sqlite_sequence")
        {
            let sroot = sq.root;
            let mut hits = AVec::new();
            let mut cursor = ScanCursor::new(sroot);
            let mut buf = AVec::new();
            while let Some(rowid) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                let v = crate::record::decode_record::<S::Error>(buf.as_slice(), 2)?;
                if matches!(v.as_slice().first(), Some(Value::Text(n)) if *n == tn) {
                    let seq = v
                        .as_slice()
                        .get(1)
                        .map(|x| OwnedValue::from_value(*x))
                        .unwrap_or(OwnedValue::Null);
                    hits.push((rowid, seq));
                }
            }
            for (rowid, seq) in hits.iter() {
                let mut vals: AVec<Value<'_>> = AVec::new();
                vals.push(Value::Text(new.as_slice()));
                vals.push(seq.as_value());
                let mut rec = AVec::new();
                encode_record(vals.as_slice(), &mut rec);
                btree::btree_delete(&mut self.conn.pager, sroot, *rowid).await?;
                btree::btree_insert(&mut self.conn.pager, sroot, *rowid, rec.as_slice()).await?;
            }
        }
        self.conn.pager.bump_schema_cookie();
        self.conn.rebuild_catalog().await?;
        // Phase 3: every view must still plan ("error in view v
        // after rename").
        let mut views = AVec::new();
        for v in self.conn.catalog.views.iter() {
            views.push(copy_vec(handle_bytes(&self.conn.catalog_arena, v.name)));
        }
        for vn in views.iter() {
            let ckpt = arena.checkpoint();
            if let Some(v) = self
                .conn
                .catalog
                .find_view(&self.conn.catalog_arena, vn.as_slice())
            {
                let v = v.clone();
                let (select, _) = self.conn.expand_view(&arena, &v)?;
                self.conn.plan_subquery(&select, &arena, &[])?;
            }
            arena.rewind(ckpt);
        }
        // Every trigger must still plan ("error in trigger t after
        // rename", v0.16, `triggers`).
        #[cfg(feature = "triggers")]
        self.conn.check_triggers(&arena)?;
        // Phase 4: DROP COLUMN rewrites every row without it.
        if let AlterOp::Drop { col, .. } = &op
            && layout.as_slice().get(*col).is_some_and(|l| l.0)
        {
            self.drop_rewrite(troot, layout.as_slice(), *col).await?;
        }
        if !self.conn.explicit_txn {
            self.conn.pager.drive_commit().await?;
        }
        Ok(())
    }

    /// The table's text edited for `op` (`own`: the altered table's
    /// own text), with the pin's checks.
    fn edit_table(
        &self,
        sql: &[u8],
        own: bool,
        tn: &[u8],
        op: &AlterOp,
        arena: &Arena,
    ) -> Result<AVec<u8>, S::Error> {
        let w = walk_table::<S::Error>(sql, arena, self.conn.cfg.limits())?;
        let mut eds = AVec::new();
        match op {
            AlterOp::Rename { new } => {
                if own {
                    eds.push(edit_for(sql, w.name, new.as_slice(), true));
                    for (_, (q, _, _)) in w.exprs.iter() {
                        if let Some((o, qn)) = q
                            && eq_ci(qn.as_slice(), tn)
                        {
                            eds.push(edit_for(sql, *o, new.as_slice(), true));
                        }
                    }
                }
                for ((o, p), _) in w.refs.iter() {
                    if eq_ci(p.as_slice(), tn) {
                        eds.push(edit_for(sql, *o, new.as_slice(), true));
                    }
                }
            }
            AlterOp::RenameCol {
                col,
                old,
                new,
                quoted,
            } => {
                let old = old.as_slice();
                if own {
                    if let Some((o, _, _)) = w.cols.as_slice().get(*col) {
                        eds.push(edit_for(sql, *o, new.as_slice(), *quoted));
                    }
                    for (o, n) in w.keys.iter().chain(w.fk_local.iter()) {
                        if eq_ci(n.as_slice(), old) {
                            eds.push(edit_for(sql, *o, new.as_slice(), *quoted));
                        }
                    }
                    for (_, r) in w.exprs.iter() {
                        if names_col(r, tn, old) {
                            eds.push(edit_for(sql, r.1, new.as_slice(), *quoted));
                        }
                    }
                }
                for ((_, p), cols) in w.refs.iter() {
                    if eq_ci(p.as_slice(), tn) {
                        for (o, n) in cols.iter() {
                            if eq_ci(n.as_slice(), old) {
                                eds.push(edit_for(sql, *o, new.as_slice(), *quoted));
                            }
                        }
                    }
                }
            }
            AlterOp::Drop { col, name } => {
                if !own {
                    return Ok(copy_vec(sql));
                }
                let name = name.as_slice();
                let Some((_, _, pk_uniq)) = w.cols.as_slice().get(*col) else {
                    return Err(err(SchemaKind::UnknownColumn));
                };
                if *pk_uniq || w.keys.iter().any(|(_, n)| eq_ci(n.as_slice(), name)) {
                    return Err(err(SchemaKind::CannotDropColumn));
                }
                // A column's definition goes up to the next column;
                // the last column's from the comma before it up to
                // the comma that opens the table constraints, or the
                // `)` (the pin's `addColOffset`).
                let k = w
                    .col_items
                    .as_slice()
                    .get(*col)
                    .copied()
                    .unwrap_or(usize::MAX);
                let last = *col + 1 == w.cols.len();
                // The comma is a token: one inside a comment or a
                // quoted name is not it, as on the pin.
                let items = w.items.as_slice();
                let commas = w.commas.as_slice();
                let (at, end) = match (items.get(k), items.get(k + 1)) {
                    (Some(&a), Some(&b)) if !last => (a, b),
                    (Some(_), next) => match (commas.get(k), next.map(|_| commas.get(k + 1))) {
                        (Some(&a), None) => (a, w.close),
                        (Some(&a), Some(Some(&b))) => (a, b),
                        _ => return Err(err(SchemaKind::UnknownColumn)),
                    },
                    _ => return Err(err(SchemaKind::UnknownColumn)),
                };
                let mut out = AVec::new();
                out.extend_from_slice(&sql[..at]);
                out.extend_from_slice(&sql[end.min(sql.len())..]);
                // What remains must not use the column.
                let nw = walk_table::<S::Error>(out.as_slice(), arena, self.conn.cfg.limits())?;
                let uses = nw.exprs.iter().any(|(_, r)| names_col(r, tn, name))
                    || nw.fk_local.iter().any(|(_, n)| eq_ci(n.as_slice(), name));
                if uses {
                    return Err(err(SchemaKind::UnknownColumn));
                }
                return Ok(out);
            }
        }
        Ok(apply(sql, eds))
    }

    /// Rewrite every row of the table rooted at `root` without
    /// stored column `col` (see the module docs).
    async fn drop_rewrite(
        &mut self,
        root: u32,
        layout: &[(bool, OwnedValue)],
        col: usize,
    ) -> Result<(), S::Error> {
        // A WITHOUT ROWID table (v0.16): its clustered records lose
        // the column in place (never a key column).
        #[cfg(feature = "without_rowid")]
        if let Some(t) = self.conn.pager.wr.table_of(root) {
            let stored = layout.iter().take(col).filter(|l| l.0).count();
            return crate::wr::drop_field(&mut self.conn.pager, t, stored).await;
        }
        let mut rowids = AVec::new();
        {
            let mut cursor = ScanCursor::new(root);
            let mut buf = AVec::new();
            while let Some(r) = cursor.next_row_into(&mut self.conn.pager, &mut buf).await? {
                rowids.push(r);
            }
        }
        let nstored = layout.iter().filter(|l| l.0).count();
        for &rowid in rowids.iter() {
            let Some(payload) = btree::seek_rowid(&mut self.conn.pager, root, rowid).await? else {
                continue;
            };
            let vals = crate::record::decode_record::<S::Error>(payload.as_slice(), nstored)?;
            let mut out: AVec<OwnedValue> = AVec::new();
            let mut s = 0usize;
            for (i, (stored, d)) in layout.iter().enumerate() {
                if !*stored {
                    continue;
                }
                if i != col {
                    out.push(match vals.as_slice().get(s) {
                        Some(v) => OwnedValue::from_value(*v),
                        None => d.clone(),
                    });
                }
                s += 1;
            }
            let mut refs: AVec<Value<'_>> = AVec::new();
            for v in out.iter() {
                refs.push(v.as_value());
            }
            let mut rec = AVec::new();
            crate::record::encode_record_plain(refs.as_slice(), &mut rec);
            btree::btree_delete(&mut self.conn.pager, root, rowid).await?;
            btree::btree_insert(&mut self.conn.pager, root, rowid, rec.as_slice()).await?;
        }
        Ok(())
    }
}

/// Add a hit for each compound ORDER BY term of `s` that names an
/// unaliased result column of its first core whose name token is a
/// hit.
fn compound_order_hits(s: &SelectStmt, arena: &Arena, hits: &mut AVec<crate::query::TraceHit>) {
    if s.compound.is_empty() {
        return;
    }
    for (e, _) in s.order_by.iter() {
        let Expr::Column { table: None, name } = e else {
            continue;
        };
        let n = handle_bytes(arena, *name);
        let item = s.items.iter().find_map(|it| match it {
            SelectItem::Expr(_, Some(a), _) if eq_ci(handle_bytes(arena, *a), n) => Some(None),
            SelectItem::Expr(Expr::Column { name: h, .. }, None, _)
                if eq_ci(handle_bytes(arena, *h), n) =>
            {
                Some(Some(*h))
            }
            _ => None,
        });
        if let Some(Some(h)) = item
            && let Some(hit) = hits.iter().find(|x| x.name == Some(h.pos)).copied()
        {
            hits.push(crate::query::TraceHit {
                name: Some(name.pos),
                col: hit.col,
                qual: None,
            });
        }
    }
}

/// `sqlite_autoindex_<old>_N` renamed with its table (other names
/// are kept).
fn auto_index_name(name: &[u8], old: &[u8], new: &[u8]) -> AVec<u8> {
    const P: &[u8] = b"sqlite_autoindex_";
    match name.get(P.len()..P.len() + old.len()) {
        Some(o) if eq_ci(&name[..P.len()], P) && eq_ci(o, old) => {
            let mut v = copy_vec(P);
            v.extend_from_slice(new);
            v.extend_from_slice(&name[P.len() + old.len()..]);
            v
        }
        _ => copy_vec(name),
    }
}

/// An index's text edited for `op` on its table `tn`.
fn edit_index<E>(
    sql: &[u8],
    tn: &[u8],
    op: &AlterOp,
    arena: &Arena,
    lim: Limits,
) -> Result<AVec<u8>, E> {
    let ((to, _), refs) = walk_index::<E>(sql, arena, lim)?;
    let mut eds = AVec::new();
    match op {
        AlterOp::Rename { new } => {
            eds.push(edit_for(sql, to, new.as_slice(), true));
            for (q, _, _) in refs.iter() {
                if let Some((o, qn)) = q
                    && eq_ci(qn.as_slice(), tn)
                {
                    eds.push(edit_for(sql, *o, new.as_slice(), true));
                }
            }
        }
        AlterOp::RenameCol {
            old, new, quoted, ..
        } => {
            for r in refs.iter() {
                if names_col(r, tn, old.as_slice()) {
                    eds.push(edit_for(sql, r.1, new.as_slice(), *quoted));
                }
            }
        }
        AlterOp::Drop { name, .. } => {
            if refs.iter().any(|r| names_col(r, tn, name.as_slice())) {
                return Err(err(SchemaKind::UnknownColumn));
            }
        }
    }
    Ok(apply(sql, eds))
}
