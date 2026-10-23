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

use super::ast::{Ast, ColumnDef, ConstraintSet, IndexColumnDef, Literal, SelectList};
use super::lexer::{self, Tok, Token};

const MAX_DEPTH: usize = 32;

fn perr<E>(offset: usize, kind: ParseKind) -> Error<E> {
    Error::Parse { offset, kind }
}

/// Resolve an arena handle to bytes.
pub fn handle_bytes(arena: &Arena, h: ABytes) -> &[u8] {
    arena
        .slice_at(h.pos as usize, h.len as usize)
        .unwrap_or(&[])
}

/// ASCII case-insensitive byte equality (SQLite identifier rules).
pub fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
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
            if self.is_kw(self.pos + 1, b"TABLE") {
                self.create_table::<E>()
            } else if self.is_kw(self.pos + 1, b"INDEX") {
                self.create_index_dispatch::<E>(false)
            } else if self.is_kw(self.pos + 1, b"UNIQUE") {
                self.create_index_dispatch::<E>(true)
            } else {
                Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
            }
        } else if self.cur_is_kw(b"DROP") {
            self.drop_index_dispatch::<E>()
        } else if self.cur_is_kw(b"INSERT") {
            self.insert::<E>()
        } else if self.cur_is_kw(b"SELECT") {
            self.select::<E>()
        } else if self.cur_is_kw(b"BEGIN") {
            self.pos += 1;
            // BEGIN [TRANSACTION]; deferred/immediate/exclusive all
            // parse — the engine has one lock level in v0.2.
            let _ =
                self.eat_kw(b"DEFERRED") || self.eat_kw(b"IMMEDIATE") || self.eat_kw(b"EXCLUSIVE");
            let _ = self.eat_kw(b"TRANSACTION");
            Ok(Ast::Begin)
        } else if self.cur_is_kw(b"COMMIT") || self.cur_is_kw(b"END") {
            self.pos += 1;
            let _ = self.eat_kw(b"TRANSACTION");
            Ok(Ast::Commit)
        } else if self.cur_is_kw(b"ROLLBACK") {
            self.pos += 1;
            let _ = self.eat_kw(b"TRANSACTION");
            // ROLLBACK TO <savepoint> is a later phase.
            if self.cur_is_kw(b"TO") {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
            Ok(Ast::Rollback)
        } else if self.at_eof() {
            Err(perr(self.cur().offset, ParseKind::UnexpectedEof))
        } else {
            Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
        }
    }

    // ---------------- CREATE / DROP INDEX (v0.3) ----------------

    /// Index DDL exists only under the `indexes` feature; without
    /// it the statements are unsupported (the tiny profile's
    /// documented exclusion, SPEC §2).
    #[cfg(feature = "indexes")]
    fn create_index_dispatch<E>(&mut self, unique: bool) -> Result<Ast, E> {
        self.create_index::<E>(unique)
    }

    #[cfg(not(feature = "indexes"))]
    fn create_index_dispatch<E>(&mut self, _unique: bool) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    #[cfg(feature = "indexes")]
    fn drop_index_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.drop_index::<E>()
    }

    #[cfg(not(feature = "indexes"))]
    fn drop_index_dispatch<E>(&mut self) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    #[cfg(feature = "indexes")]
    fn create_index<E>(&mut self, unique: bool) -> Result<Ast, E> {
        self.expect_kw::<E>(b"CREATE")?;
        if unique {
            self.expect_kw::<E>(b"UNIQUE")?;
        }
        self.expect_kw::<E>(b"INDEX")?;
        let if_not_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"NOT")?;
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        let name = self.name::<E>()?;
        self.expect_kw::<E>(b"ON")?;
        let table = self.name::<E>()?;
        self.expect::<E>(Tok::LParen)?;
        let mut columns: AVec<IndexColumnDef> = AVec::new();
        loop {
            let col = self.name::<E>()?;
            // An expression index column looks like `abs(a)` or
            // `(a+1)`: a '(' right here (or as the item start,
            // caught because name() rejects '(') is outside v0.3.
            if self.cur().tok == Tok::LParen {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
            if self.eat_kw(b"COLLATE") {
                let coll = self.name::<E>()?;
                if !eq_ci(handle_bytes(self.arena, coll), b"BINARY") {
                    // Only the BINARY collation exists in hipp.
                    return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
                }
            }
            let desc = if self.eat_kw(b"DESC") {
                true
            } else {
                let _ = self.eat_kw(b"ASC");
                false
            };
            columns.push(IndexColumnDef { name: col, desc });
            if self.eat(Tok::Comma) {
                continue;
            }
            self.expect::<E>(Tok::RParen)?;
            break;
        }
        // Partial indexes (WHERE ...) are a later phase.
        if self.cur_is_kw(b"WHERE") {
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        Ok(Ast::CreateIndex {
            name,
            table,
            unique,
            if_not_exists,
            columns,
        })
    }

    #[cfg(feature = "indexes")]
    fn drop_index<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"DROP")?;
        if !self.eat_kw(b"INDEX") {
            // DROP TABLE / DROP VIEW are not in any hipp phase yet.
            return Err(perr(self.cur().offset, ParseKind::UnsupportedStatement));
        }
        let if_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        let name = self.name::<E>()?;
        Ok(Ast::DropIndex { name, if_exists })
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
        let mut pk_desc_names: AVec<ABytes> = AVec::new();
        let mut sets: AVec<ConstraintSet> = AVec::new();
        for (from, to) in items.iter() {
            self.create_item::<E>(
                *from,
                *to,
                &mut columns,
                &mut pk_names,
                &mut pk_desc_names,
                &mut sets,
            )?;
        }
        if columns.is_empty() {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        }
        // Mark PK columns (candidate rowid-alias semantics: a PK
        // column written with DESC is not marked — see ast docs).
        for target in pk_names.iter() {
            let desc = pk_desc_names.iter().any(|d| {
                eq_ci(
                    handle_bytes(self.arena, *d),
                    handle_bytes(self.arena, *target),
                )
            });
            for c in columns.as_mut_slice().iter_mut() {
                if eq_ci(
                    handle_bytes(self.arena, c.name),
                    handle_bytes(self.arena, *target),
                ) {
                    c.pk_desc = desc;
                    c.primary_key = !desc;
                }
            }
        }
        let mut pk_columns: AVec<ABytes> = AVec::new();
        let mut pk_sets = 0usize;
        for s in sets.iter() {
            if s.primary_key {
                pk_sets += 1;
                for c in s.columns.iter() {
                    pk_columns.push(c.name);
                }
            }
        }
        if pk_sets > 1 {
            // SQLite: "table has more than one primary key".
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        // Trailing clauses: WITHOUT ROWID is outside v0.1.
        if self.cur_is_kw(b"WITHOUT") {
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        Ok(Ast::CreateTable {
            name,
            if_not_exists,
            columns,
            pk_columns,
            constraint_sets: sets,
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

    /// Parse a `(col [ASC|DESC], ...)` list starting at token
    /// `from` (the '(' is consumed by the scan); returns the column
    /// defs. Used by table-level PRIMARY KEY / UNIQUE items.
    fn constraint_columns(&self, mut i: usize, to: usize) -> AVec<IndexColumnDef> {
        let mut out = AVec::new();
        if i < to && self.toks.as_slice()[i].tok == Tok::LParen {
            i += 1;
        }
        while i < to {
            let Some((h, _, _)) = self.ident_at(i) else {
                break;
            };
            i += 1;
            let mut desc = false;
            if self.is_kw(i, b"DESC") {
                desc = true;
                i += 1;
            } else if self.is_kw(i, b"ASC") {
                i += 1;
            }
            // A COLLATE clause may intervene; ignore its name.
            if self.is_kw(i, b"COLLATE") {
                i += 2;
            }
            out.push(IndexColumnDef { name: h, desc });
            if i < to && self.toks.as_slice()[i].tok == Tok::Comma {
                i += 1;
                continue;
            }
            break;
        }
        out
    }

    fn create_item<E>(
        &mut self,
        mut from: usize,
        to: usize,
        columns: &mut AVec<ColumnDef>,
        pk_names: &mut AVec<ABytes>,
        pk_desc_names: &mut AVec<ABytes>,
        sets: &mut AVec<ConstraintSet>,
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
            let cols = self.constraint_columns(i, to);
            for c in cols.iter() {
                pk_names.push(c.name);
                if c.desc {
                    pk_desc_names.push(c.name);
                }
            }
            sets.push(ConstraintSet {
                primary_key: true,
                columns: cols,
            });
            return Ok(());
        }
        if self.is_kw(from, b"UNIQUE")
            && from + 1 < to
            && self.toks.as_slice()[from + 1].tok == Tok::LParen
        {
            // Table-level UNIQUE (col, …) (v0.3: enforced via an
            // implicit index, like SQLite).
            let cols = self.constraint_columns(from + 1, to);
            sets.push(ConstraintSet {
                primary_key: false,
                columns: cols,
            });
            return Ok(());
        }
        if self.is_kw(from, b"CHECK")
            || self.is_kw(from, b"FOREIGN")
            || self.is_kw(from, b"KEY")
            || self.is_kw(from, b"INDEX")
        {
            // Parsed, not enforced in v0.1 (see module docs).
            return Ok(());
        }
        // Column definition: first token is the name.
        let Some((col_name, _, _)) = self.ident_at(from) else {
            return Err(perr(
                self.toks.as_slice()[from].offset,
                ParseKind::UnexpectedToken,
            ));
        };
        // Declared type: tokens after the name up to the first
        // depth-0 constraint keyword; the text is the SQL span.
        let mut depth = 0usize;
        let mut decl_end_tok = to;
        let mut pk = false;
        let mut pk_desc = false;
        let mut unique = false;
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
                if self.is_kw(i, b"UNIQUE") {
                    unique = true;
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
            pk_desc,
            unique,
            default,
        });
        // Constraint sets in statement order (v0.3): an inline
        // PRIMARY KEY / UNIQUE is a one-column set at this item's
        // position.
        if pk {
            pk_names.push(col_name);
            if pk_desc {
                pk_desc_names.push(col_name);
            }
            let mut cols = AVec::new();
            cols.push(IndexColumnDef {
                name: col_name,
                desc: pk_desc,
            });
            sets.push(ConstraintSet {
                primary_key: true,
                columns: cols,
            });
        }
        if unique {
            let mut cols = AVec::new();
            cols.push(IndexColumnDef {
                name: col_name,
                desc: false,
            });
            sets.push(ConstraintSet {
                primary_key: false,
                columns: cols,
            });
        }
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
        // v0.2: one or more parenthesised rows, comma-separated.
        let mut rows = AVec::new();
        loop {
            self.expect::<E>(Tok::LParen)?;
            let mut row = AVec::new();
            loop {
                let (lit, used) = self
                    .literal_at(self.pos)
                    .ok_or(perr(self.cur().offset, ParseKind::UnexpectedToken))?;
                row.push(lit);
                self.pos += used;
                if self.eat(Tok::Comma) {
                    continue;
                }
                self.expect::<E>(Tok::RParen)?;
                break;
            }
            rows.push(row);
            if self.eat(Tok::Comma) {
                continue;
            }
            break;
        }
        Ok(Ast::Insert {
            table,
            columns,
            rows,
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
        // v0.2: optional WHERE <column> = <literal>. Any other
        // predicate shape is rejected here; whether <column> is the
        // rowid is a plan-time decision (the executor seam).
        let mut predicate = None;
        if self.eat_kw(b"WHERE") {
            let col = self.name::<E>()?;
            if !self.eat(Tok::Eq) {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
            let (lit, used) = self
                .literal_at(self.pos)
                .ok_or(perr(self.cur().offset, ParseKind::UnsupportedClause))?;
            self.pos += used;
            predicate = Some((col, lit));
        }
        Ok(Ast::Select {
            table,
            list,
            predicate,
        })
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
