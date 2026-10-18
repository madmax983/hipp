//! Recursive-descent parser for the v0.1 grammar (SPEC §5.6).
//!
//! The `CREATE TABLE` path is deliberately more tolerant than the
//! generator grammar: `sqlite_schema` rows written by real SQLite
//! must re-parse (§5.5), so column/table constraints SQLite emits
//! (`NOT NULL`, `CHECK`, `DEFAULT`, foreign keys, …) are accepted
//! syntactically. v0.1 *enforces* only `PRIMARY KEY` (for the
//! rowid-alias rule); other constraints are parsed and not enforced
//! — a recorded v0.1 dialect limitation, widened in v0.2.

use crate::arena::{ABytes, AVec, Arena};
use crate::error::{Error, ParseKind, Result};

use super::ast::{Ast, ColumnDef, Literal, SelectList};
use super::lexer::{self, Tok, Token};

const MAX_DEPTH: usize = 32;

fn perr<E>(offset: usize, kind: ParseKind) -> Error<E> {
    Error::Parse { offset, kind }
}

/// Resolve an arena handle to bytes.
pub fn handle_bytes<'a>(arena: &'a Arena, h: ABytes) -> &'a [u8] {
    arena.slice_at(h.pos as usize, h.len as usize).unwrap_or(&[])
}

/// ASCII case-insensitive byte equality (SQLite identifier rules).
pub fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
}

struct Parser<'a> {
    toks: AVec<Token>,
    pos: usize,
    limit: usize,
    arena: &'a Arena,
    sql: &'a [u8],
}

pub fn parse<E>(sql: &[u8], arena: &Arena) -> Result<Ast, E> {
    let toks = lexer::lex::<E>(sql, arena)?;
    let mut p = Parser {
        limit: toks.len(),
        toks,
        pos: 0,
        arena,
        sql,
    };
    let ast = p.statement::<E>()?;
    p.expect_end::<E>()?;
    Ok(ast)
}

impl<'a> Parser<'a> {
    fn cur(&self) -> Token {
        let i = self.pos.min(self.limit - 1);
        self.toks.as_slice()[i]
    }

    fn at_eof(&self) -> bool {
        matches!(self.cur().tok, Tok::Eof) || self.pos >= self.limit
    }

    fn ident_at(&self, i: usize) -> Option<(ABytes, bool, usize)> {
        if i >= self.limit {
            return None;
        }
        match self.toks.as_slice()[i].tok {
            Tok::Ident(h, q) => Some((h, q, self.toks.as_slice()[i].offset)),
            _ => None,
        }
    }

    fn is_kw(&self, i: usize, word: &[u8]) -> bool {
        match self.ident_at(i) {
            Some((h, false, _)) => eq_ci(handle_bytes(self.arena, h), word),
            _ => false,
        }
    }

    fn cur_is_kw(&self, word: &[u8]) -> bool {
        self.is_kw(self.pos, word)
    }

    fn eat_kw(&mut self, word: &[u8]) -> bool {
        if self.cur_is_kw(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw<E>(&mut self, word: &[u8]) -> Result<(), E> {
        if self.eat_kw(word) {
            Ok(())
        } else {
            Err(perr(self.cur().offset, ParseKind::UnexpectedToken))
        }
    }

    fn eat(&mut self, t: Tok) -> bool {
        if self.cur().tok == t {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect<E>(&mut self, t: Tok) -> Result<(), E> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(perr(self.cur().offset, ParseKind::UnexpectedToken))
        }
    }

    /// A table/column name: any identifier token (quoted or not).
    fn name<E>(&mut self) -> Result<ABytes, E> {
        match self.cur().tok {
            Tok::Ident(h, _) => {
                self.pos += 1;
                // Accept (and discard) a schema qualifier: `main.t`.
                if self.cur().tok == Tok::Dot {
                    self.pos += 1;
                    return self.name::<E>();
                }
                Ok(h)
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    fn expect_end<E>(&mut self) -> Result<(), E> {
        self.eat(Tok::Semi);
        if self.at_eof() {
            Ok(())
        } else {
            Err(perr(self.cur().offset, ParseKind::TrailingInput))
        }
    }

    fn statement<E>(&mut self) -> Result<Ast, E> {
        if self.cur_is_kw(b"CREATE") {
            self.create_table::<E>()
        } else if self.cur_is_kw(b"INSERT") {
            self.insert::<E>()
        } else if self.cur_is_kw(b"SELECT") {
            self.select::<E>()
        } else if self.at_eof() {
            Err(perr(self.cur().offset, ParseKind::UnexpectedEof))
        } else {
            Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
        }
    }

    // ---------------- CREATE TABLE ----------------

    fn create_table<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"CREATE")?;
        self.expect_kw::<E>(b"TABLE")?;
        let if_not_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"NOT")?;
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        let name = self.name::<E>()?;
        self.expect::<E>(Tok::LParen)?;
        // Split the parenthesised body into top-level comma items.
        let items = self.split_items::<E>()?;
        let mut columns: AVec<ColumnDef> = AVec::new();
        let mut pk_names: AVec<ABytes> = AVec::new();
        for (from, to) in items.iter() {
            self.create_item::<E>(*from, *to, &mut columns, &mut pk_names)?;
        }
        if columns.is_empty() {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        }
        if pk_names.len() == 1 {
            let target = pk_names.as_slice()[0];
            for c in columns.as_mut_slice().iter_mut() {
                if eq_ci(handle_bytes(self.arena, c.name), handle_bytes(self.arena, target)) {
                    c.primary_key = true;
                }
            }
        }
        // Trailing clauses: WITHOUT ROWID is outside v0.1.
        if self.cur_is_kw(b"WITHOUT") {
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        Ok(Ast::CreateTable {
            name,
            if_not_exists,
            columns,
        })
    }

    /// Consume tokens up to the matching `)` of an already-eaten
    /// `(`, returning (from, to) token-index ranges per top-level
    /// comma-separated item.
    fn split_items<E>(&mut self) -> Result<AVec<(usize, usize)>, E> {
        let mut items = AVec::new();
        let mut depth = 0usize;
        let mut start = self.pos;
        loop {
            let t = self.cur();
            match t.tok {
                Tok::Eof => return Err(perr(t.offset, ParseKind::UnexpectedEof)),
                Tok::LParen => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err(perr(t.offset, ParseKind::RecursionLimit));
                    }
                    self.pos += 1;
                }
                Tok::RParen => {
                    if depth == 0 {
                        items.push((start, self.pos));
                        self.pos += 1;
                        return Ok(items);
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                Tok::Comma if depth == 0 => {
                    items.push((start, self.pos));
                    self.pos += 1;
                    start = self.pos;
                }
                _ => self.pos += 1,
            }
        }
    }

    fn create_item<E>(
        &mut self,
        mut from: usize,
        to: usize,
        columns: &mut AVec<ColumnDef>,
        pk_names: &mut AVec<ABytes>,
    ) -> Result<(), E> {
        // CONSTRAINT name … — skip the constraint's name.
        if self.is_kw(from, b"CONSTRAINT") {
            from += 2;
        }
        if from >= to {
            return Ok(());
        }
        if self.is_kw(from, b"PRIMARY") {
            // Table-level PRIMARY KEY (col, …)
            let mut i = from + 1;
            if self.is_kw(i, b"KEY") {
                i += 1;
            }
            if i < to && self.toks.as_slice()[i].tok == Tok::LParen {
                i += 1;
                while i < to {
                    if let Some((h, _, _)) = self.ident_at(i) {
                        pk_names.push(h);
                    }
                    i += 1;
                    if i < to && self.toks.as_slice()[i].tok == Tok::Comma {
                        i += 1;
                    }
                }
            }
            return Ok(());
        }
        if self.is_kw(from, b"UNIQUE")
            || self.is_kw(from, b"CHECK")
            || self.is_kw(from, b"FOREIGN")
            || self.is_kw(from, b"KEY")
            || self.is_kw(from, b"INDEX")
        {
            // Parsed, not enforced in v0.1 (see module docs).
            return Ok(());
        }
        // Column definition: first token is the name.
        let Some((col_name, _, _)) = self.ident_at(from) else {
            return Err(perr(self.toks.as_slice()[from].offset, ParseKind::UnexpectedToken));
        };
        // Declared type: tokens after the name up to the first
        // depth-0 constraint keyword; the text is the SQL span.
        let mut depth = 0usize;
        let mut decl_end_tok = to;
        let mut pk = false;
        let mut pk_desc = false;
        let mut default: Option<Literal> = None;
        let mut i = from + 1;
        while i < to {
            let t = self.toks.as_slice()[i];
            match t.tok {
                Tok::LParen => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err(perr(t.offset, ParseKind::RecursionLimit));
                    }
                }
                Tok::RParen => depth = depth.saturating_sub(1),
                _ => {}
            }
            if depth == 0 {
                if self.is_kw(i, b"PRIMARY") && self.is_kw(i + 1, b"KEY") {
                    pk = true;
                    decl_end_tok = decl_end_tok.min(i);
                    if self.is_kw(i + 2, b"DESC") {
                        pk_desc = true;
                    }
                }
                if self.is_kw(i, b"DEFAULT") {
                    decl_end_tok = decl_end_tok.min(i);
                    default = self.default_literal(i + 1, to);
                }
                if self.is_kw(i, b"NOT")
                    || self.is_kw(i, b"NULL")
                    || self.is_kw(i, b"UNIQUE")
                    || self.is_kw(i, b"CHECK")
                    || self.is_kw(i, b"REFERENCES")
                    || self.is_kw(i, b"COLLATE")
                    || self.is_kw(i, b"GENERATED")
                    || self.is_kw(i, b"CONSTRAINT")
                {
                    decl_end_tok = decl_end_tok.min(i);
                }
            }
            i += 1;
        }
        let _ = pk_desc; // folded into primary_key handling below
        let decl = if decl_end_tok > from + 1 {
            let start_off = self.toks.as_slice()[from + 1].offset;
            let end_off = if decl_end_tok < to {
                self.toks.as_slice()[decl_end_tok].offset
            } else {
                // End of item: use the end offset of the last token.
                self.item_end_offset(decl_end_tok)
            };
            let text = trim(&self.sql[start_off..end_off.min(self.sql.len())]);
            self.arena.alloc_copy_handle(text)?
        } else {
            ABytes::EMPTY
        };
        columns.push(ColumnDef {
            name: col_name,
            decl,
            primary_key: pk && !pk_desc,
            default,
        });
        Ok(())
    }

    fn item_end_offset(&self, tok_idx: usize) -> usize {
        // Offset just past the last significant token before tok_idx:
        // approximate with the next token's offset (items are followed
        // by ',' or ')' whose offsets bound the span), else SQL end.
        if tok_idx < self.toks.len() {
            self.toks.as_slice()[tok_idx].offset
        } else {
            self.sql.len()
        }
    }

    /// Parse a DEFAULT literal starting at token `i` (best effort:
    /// non-literal defaults like CURRENT_TIMESTAMP yield None).
    fn default_literal(&self, mut i: usize, to: usize) -> Option<Literal> {
        if i < to && self.toks.as_slice()[i].tok == Tok::LParen {
            i += 1;
        }
        self.literal_at(i).map(|(lit, _)| lit)
    }

    fn literal_at(&self, i: usize) -> Option<(Literal, usize)> {
        if i >= self.limit {
            return None;
        }
        let t = self.toks.as_slice()[i];
        match t.tok {
            Tok::Integer(v) => Some((Literal::Integer(v), 1)),
            Tok::Real(f) => Some((Literal::Real(f), 1)),
            Tok::Text(h) => Some((Literal::Text(h), 1)),
            Tok::Blob(h) => Some((Literal::Blob(h), 1)),
            Tok::Minus => match self.literal_at(i + 1) {
                Some((Literal::Integer(v), n)) => Some((Literal::Integer(v.wrapping_neg()), n + 1)),
                Some((Literal::Real(f), n)) => Some((Literal::Real(-f), n + 1)),
                _ => None,
            },
            Tok::Plus => self.literal_at(i + 1).map(|(l, n)| (l, n + 1)),
            Tok::Ident(h, false) => {
                let b = handle_bytes(self.arena, h);
                if eq_ci(b, b"NULL") {
                    Some((Literal::Null, 1))
                } else if eq_ci(b, b"TRUE") {
                    Some((Literal::Integer(1), 1))
                } else if eq_ci(b, b"FALSE") {
                    Some((Literal::Integer(0), 1))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // ---------------- INSERT ----------------

    fn insert<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"INSERT")?;
        self.expect_kw::<E>(b"INTO")?;
        let table = self.name::<E>()?;
        let mut columns = None;
        if self.eat(Tok::LParen) {
            let mut cols = AVec::new();
            loop {
                cols.push(self.name::<E>()?);
                if self.eat(Tok::Comma) {
                    continue;
                }
                self.expect::<E>(Tok::RParen)?;
                break;
            }
            columns = Some(cols);
        }
        self.expect_kw::<E>(b"VALUES")?;
        self.expect::<E>(Tok::LParen)?;
        let mut values = AVec::new();
        loop {
            let (lit, used) = self
                .literal_at(self.pos)
                .ok_or(perr(self.cur().offset, ParseKind::UnexpectedToken))?;
            values.push(lit);
            self.pos += used;
            if self.eat(Tok::Comma) {
                continue;
            }
            self.expect::<E>(Tok::RParen)?;
            break;
        }
        Ok(Ast::Insert {
            table,
            columns,
            values,
        })
    }

    // ---------------- SELECT ----------------

    fn select<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"SELECT")?;
        let list = if self.eat(Tok::Star) {
            SelectList::All
        } else {
            let mut cols = AVec::new();
            loop {
                cols.push(self.name::<E>()?);
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
            SelectList::Columns(cols)
        };
        self.expect_kw::<E>(b"FROM")?;
        let table = self.name::<E>()?;
        Ok(Ast::Select { table, list })
    }
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
