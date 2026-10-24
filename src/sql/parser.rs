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

use super::ast::{
    Ast, BinOp, ColumnDef, ConstraintSet, Expr, FromSource, IndexColumnDef, JoinKind, Literal,
    SelectItem, SelectStmt,
};
use alloc::boxed::Box;

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
            } else if self.is_kw(self.pos + 1, b"VIEW") {
                self.create_view_dispatch::<E>()
            } else {
                Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
            }
        } else if self.cur_is_kw(b"DROP") {
            self.drop_dispatch::<E>()
        } else if self.cur_is_kw(b"ALTER") {
            self.alter_dispatch::<E>()
        } else if self.cur_is_kw(b"INSERT") {
            self.insert::<E>()
        } else if self.cur_is_kw(b"UPDATE") {
            self.update_dispatch::<E>()
        } else if self.cur_is_kw(b"DELETE") {
            self.delete_dispatch::<E>()
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

    /// DROP dispatch: VIEW (v0.4), TABLE (v0.5), or INDEX (v0.3).
    fn drop_dispatch<E>(&mut self) -> Result<Ast, E> {
        if self.is_kw(self.pos + 1, b"VIEW") {
            self.drop_view_dispatch::<E>()
        } else if self.is_kw(self.pos + 1, b"TABLE") {
            self.drop_table_dispatch::<E>()
        } else {
            self.drop_index_dispatch::<E>()
        }
    }

    // ---------------- v0.5 statement shims ----------------
    // UPDATE, DELETE and DROP TABLE exist only under the `v05`
    // feature (the tiny profile's documented exclusion, SPEC §2);
    // the parsers live at the bottom of this file behind the
    // same cfg.

    #[cfg(feature = "v05")]
    fn update_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.update_stmt::<E>()
    }

    #[cfg(not(feature = "v05"))]
    fn update_dispatch<E>(&mut self) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    #[cfg(feature = "v05")]
    fn delete_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.delete_stmt::<E>()
    }

    #[cfg(not(feature = "v05"))]
    fn delete_dispatch<E>(&mut self) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    #[cfg(feature = "v05")]
    fn drop_table_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.drop_table::<E>()
    }

    #[cfg(not(feature = "v05"))]
    fn drop_table_dispatch<E>(&mut self) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    // ---------------- v0.4 statement shims ----------------
    // ALTER TABLE ADD COLUMN, CREATE VIEW and DROP VIEW exist
    // only under the `v04` feature (the tiny profile's documented
    // exclusion, SPEC §2); the parsers live at the bottom of this
    // file behind the same cfg.

    #[cfg(feature = "v04")]
    fn create_view_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.create_view::<E>()
    }

    #[cfg(not(feature = "v04"))]
    fn create_view_dispatch<E>(&mut self) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    #[cfg(feature = "v04")]
    fn drop_view_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.drop_view::<E>()
    }

    #[cfg(not(feature = "v04"))]
    fn drop_view_dispatch<E>(&mut self) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    #[cfg(feature = "v04")]
    fn alter_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.alter_table::<E>()
    }

    #[cfg(not(feature = "v04"))]
    fn alter_dispatch<E>(&mut self) -> Result<Ast, E> {
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
        let mut not_null = false;
        let mut default_nonconst = false;
        let mut generated = false;
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
                    if default.is_none() {
                        // A DEFAULT whose value is not a plain
                        // literal (CURRENT_TIMESTAMP, a call, ...).
                        default_nonconst = true;
                    }
                }
                if self.is_kw(i, b"NOT") && self.is_kw(i + 1, b"NULL") {
                    not_null = true;
                }
                if self.is_kw(i, b"GENERATED") {
                    generated = true;
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
            not_null,
            default_nonconst,
            generated,
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
        // v0.4: each value is an expression (a plain literal under
        // the pre-v0.4 grammar).
        let mut rows = AVec::new();
        loop {
            self.expect::<E>(Tok::LParen)?;
            let mut row = AVec::new();
            loop {
                row.push(self.value_expr::<E>()?);
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

    /// One INSERT value: a full expression under `v04`, a plain
    /// literal otherwise.
    #[cfg(feature = "v04")]
    fn value_expr<E>(&mut self) -> Result<Expr, E> {
        self.parse_expr::<E>()
    }

    #[cfg(not(feature = "v04"))]
    fn value_expr<E>(&mut self) -> Result<Expr, E> {
        let (lit, used) = self
            .literal_at(self.pos)
            .ok_or(perr(self.cur().offset, ParseKind::UnexpectedToken))?;
        self.pos += used;
        Ok(Expr::Literal(lit))
    }

    // ---------------- SELECT ----------------

    fn select<E>(&mut self) -> Result<Ast, E> {
        #[cfg(feature = "v04")]
        {
            Ok(Ast::Select(self.select_rich::<E>()?))
        }
        #[cfg(not(feature = "v04"))]
        {
            self.select_simple::<E>()
        }
    }

    /// The pre-v0.4 SELECT grammar (kept for the tiny profile):
    /// `SELECT * | col, ... FROM t [WHERE col = literal]`, in the
    /// v0.4 AST shape.
    #[cfg(not(feature = "v04"))]
    fn select_simple<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"SELECT")?;
        let mut items = AVec::new();
        if self.eat(Tok::Star) {
            items.push(SelectItem::Star);
        } else {
            loop {
                let n = self.name::<E>()?;
                items.push(SelectItem::Expr(
                    Expr::Column {
                        table: None,
                        name: n,
                    },
                    None,
                    n,
                ));
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect_kw::<E>(b"FROM")?;
        let table = self.name::<E>()?;
        let mut from = AVec::new();
        from.push(FromSource {
            table,
            alias: None,
            kind: JoinKind::Inner,
            on: None,
        });
        // v0.2: optional WHERE <column> = <literal>. Any other
        // predicate shape is rejected here; whether <column> is the
        // rowid is a plan-time decision (the executor seam).
        let mut filter = None;
        if self.eat_kw(b"WHERE") {
            let col = self.name::<E>()?;
            if !self.eat(Tok::Eq) {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
            let (lit, used) = self
                .literal_at(self.pos)
                .ok_or(perr(self.cur().offset, ParseKind::UnsupportedClause))?;
            self.pos += used;
            filter = Some(Expr::Binary(
                BinOp::Eq,
                Box::new(Expr::Column {
                    table: None,
                    name: col,
                }),
                Box::new(Expr::Literal(lit)),
            ));
        }
        Ok(Ast::Select(SelectStmt {
            distinct: false,
            items,
            from,
            filter,
            group_by: AVec::new(),
            having: None,
            order_by: AVec::new(),
            limit: None,
            offset: 0,
        }))
    }
}

// ================= v0.4 grammar (feature = "v04") =================

#[cfg(feature = "v04")]
impl<'a> Parser<'a> {
    /// True when the current token is an unquoted identifier whose
    /// text is a clause keyword that must not be eaten as a bare
    /// alias.
    fn at_clause_kw(&self) -> bool {
        const KWS: &[&[u8]] = &[
            b"FROM",
            b"WHERE",
            b"GROUP",
            b"HAVING",
            b"ORDER",
            b"LIMIT",
            b"OFFSET",
            b"JOIN",
            b"INNER",
            b"LEFT",
            b"CROSS",
            b"OUTER",
            b"ON",
            b"UNION",
            b"EXCEPT",
            b"INTERSECT",
        ];
        match self.ident_at(self.pos) {
            Some((h, false, _)) => {
                let b = handle_bytes(self.arena, h);
                KWS.iter().any(|k| eq_ci(b, k))
            }
            _ => false,
        }
    }

    /// An optional `[AS] name` alias: a quoted identifier, or a
    /// bare identifier that is not a clause keyword.
    fn opt_alias(&mut self) -> Option<ABytes> {
        if self.eat_kw(b"AS") {
            return match self.cur().tok {
                Tok::Ident(h, _) => {
                    self.pos += 1;
                    Some(h)
                }
                _ => None,
            };
        }
        match self.cur().tok {
            Tok::Ident(h, quoted) => {
                if quoted || !self.at_clause_kw() {
                    self.pos += 1;
                    Some(h)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // ---------- expression parser (SQLite precedence) ----------
    // OR < AND < NOT < comparison < +,- < *,/,% < || < unary.

    pub(crate) fn parse_expr<E>(&mut self) -> Result<Expr, E> {
        self.expr_or::<E>()
    }

    fn expr_or<E>(&mut self) -> Result<Expr, E> {
        let mut l = self.expr_and::<E>()?;
        while self.eat_kw(b"OR") {
            let r = self.expr_and::<E>()?;
            l = Expr::Binary(BinOp::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn expr_and<E>(&mut self) -> Result<Expr, E> {
        let mut l = self.expr_not::<E>()?;
        while self.eat_kw(b"AND") {
            let r = self.expr_not::<E>()?;
            l = Expr::Binary(BinOp::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn expr_not<E>(&mut self) -> Result<Expr, E> {
        if self.eat_kw(b"NOT") {
            let e = self.expr_not::<E>()?;
            return Ok(Expr::Not(Box::new(e)));
        }
        self.expr_cmp::<E>()
    }

    fn expr_cmp<E>(&mut self) -> Result<Expr, E> {
        let mut l = self.expr_add::<E>()?;
        loop {
            // Postfix ISNULL / NOTNULL.
            if self.eat_kw(b"ISNULL") {
                l = Expr::IsNull(Box::new(l), false);
                continue;
            }
            if self.eat_kw(b"NOTNULL") {
                l = Expr::IsNull(Box::new(l), true);
                continue;
            }
            if self.cur_is_kw(b"IS") {
                self.pos += 1;
                let neg = self.eat_kw(b"NOT");
                let r = self.expr_add::<E>()?;
                l = Expr::Binary(
                    if neg { BinOp::IsNot } else { BinOp::Is },
                    Box::new(l),
                    Box::new(r),
                );
                continue;
            }
            let neg = if self.cur_is_kw(b"NOT")
                && (self.is_kw(self.pos + 1, b"IN")
                    || self.is_kw(self.pos + 1, b"LIKE")
                    || self.is_kw(self.pos + 1, b"BETWEEN"))
            {
                self.pos += 1;
                true
            } else {
                false
            };
            if self.eat_kw(b"IN") {
                self.expect::<E>(Tok::LParen)?;
                if self.cur_is_kw(b"SELECT") {
                    return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
                }
                let mut list = AVec::new();
                if self.cur().tok != Tok::RParen {
                    loop {
                        list.push(self.parse_expr::<E>()?);
                        if self.eat(Tok::Comma) {
                            continue;
                        }
                        break;
                    }
                }
                self.expect::<E>(Tok::RParen)?;
                l = Expr::InList(Box::new(l), list, neg);
                continue;
            }
            if self.eat_kw(b"LIKE") {
                let r = self.expr_add::<E>()?;
                l = Expr::Like(Box::new(l), Box::new(r), neg);
                continue;
            }
            if self.eat_kw(b"BETWEEN") {
                let lo = self.expr_add::<E>()?;
                self.expect_kw::<E>(b"AND")?;
                let hi = self.expr_add::<E>()?;
                l = Expr::Between(Box::new(l), Box::new(lo), Box::new(hi), neg);
                continue;
            }
            if neg {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
            let op = match self.cur().tok {
                Tok::Eq => Some(BinOp::Eq),
                Tok::Ne => Some(BinOp::Ne),
                Tok::Lt => Some(BinOp::Lt),
                Tok::Le => Some(BinOp::Le),
                Tok::Gt => Some(BinOp::Gt),
                Tok::Ge => Some(BinOp::Ge),
                _ => None,
            };
            let Some(op) = op else { break };
            self.pos += 1;
            // `==` is spelled with two Eq tokens by the lexer;
            // accept it.
            if op == BinOp::Eq {
                let _ = self.eat(Tok::Eq);
            }
            let r = self.expr_add::<E>()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn expr_add<E>(&mut self) -> Result<Expr, E> {
        let mut l = self.expr_mul::<E>()?;
        loop {
            let op = match self.cur().tok {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let r = self.expr_mul::<E>()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn expr_mul<E>(&mut self) -> Result<Expr, E> {
        let mut l = self.expr_concat::<E>()?;
        loop {
            let op = match self.cur().tok {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let r = self.expr_concat::<E>()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn expr_concat<E>(&mut self) -> Result<Expr, E> {
        let mut l = self.expr_unary::<E>()?;
        while self.eat(Tok::Concat) {
            let r = self.expr_unary::<E>()?;
            l = Expr::Binary(BinOp::Concat, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn expr_unary<E>(&mut self) -> Result<Expr, E> {
        // Signed literals fold at this level (literal_at handles
        // the sign itself).
        if let Some((lit, used)) = self.literal_at(self.pos) {
            self.pos += used;
            return Ok(Expr::Literal(lit));
        }
        if self.eat(Tok::Minus) {
            let e = self.expr_unary::<E>()?;
            return Ok(Expr::Neg(Box::new(e)));
        }
        if self.eat(Tok::Plus) {
            return self.expr_unary::<E>();
        }
        self.expr_primary::<E>()
    }

    fn expr_primary<E>(&mut self) -> Result<Expr, E> {
        if self.eat(Tok::LParen) {
            if self.cur_is_kw(b"SELECT") {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
            let e = self.parse_expr::<E>()?;
            self.expect::<E>(Tok::RParen)?;
            return Ok(e);
        }
        if self.cur_is_kw(b"CASE") {
            self.pos += 1;
            let base = if self.cur_is_kw(b"WHEN") {
                None
            } else {
                Some(Box::new(self.parse_expr::<E>()?))
            };
            let mut whens = AVec::new();
            while self.eat_kw(b"WHEN") {
                let w = self.parse_expr::<E>()?;
                self.expect_kw::<E>(b"THEN")?;
                let t = self.parse_expr::<E>()?;
                whens.push((w, t));
            }
            if whens.is_empty() {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
            let otherwise = if self.eat_kw(b"ELSE") {
                Some(Box::new(self.parse_expr::<E>()?))
            } else {
                None
            };
            self.expect_kw::<E>(b"END")?;
            return Ok(Expr::Case {
                base,
                whens,
                otherwise,
            });
        }
        match self.cur().tok {
            Tok::Ident(h, quoted) => {
                let nb = handle_bytes(self.arena, h);
                // Function call: ident followed by '('.
                if !quoted
                    && self.toks.as_slice().get(self.pos + 1).map(|t| t.tok) == Some(Tok::LParen)
                {
                    if eq_ci(nb, b"CAST") {
                        self.pos += 2; // name + '('
                        let e = self.parse_expr::<E>()?;
                        self.expect_kw::<E>(b"AS")?;
                        // Type name: raw SQL slice up to ')'.
                        let start = self.cur().offset;
                        let mut depth = 0usize;
                        while !self.at_eof() {
                            match self.cur().tok {
                                Tok::LParen => depth += 1,
                                Tok::RParen => {
                                    if depth == 0 {
                                        break;
                                    }
                                    depth -= 1;
                                }
                                Tok::Eof => break,
                                _ => {}
                            }
                            self.pos += 1;
                        }
                        let end = self.cur().offset;
                        let ty = self
                            .arena
                            .alloc_copy_handle(trim(&self.sql[start..end.min(self.sql.len())]))?;
                        self.expect::<E>(Tok::RParen)?;
                        return Ok(Expr::Cast(Box::new(e), ty));
                    }
                    let fname = h;
                    self.pos += 2; // name + '('
                    let star = self.eat(Tok::Star);
                    let distinct = !star && self.eat_kw(b"DISTINCT");
                    let mut args = AVec::new();
                    if !star && self.cur().tok != Tok::RParen {
                        loop {
                            args.push(self.parse_expr::<E>()?);
                            if self.eat(Tok::Comma) {
                                continue;
                            }
                            break;
                        }
                    }
                    self.expect::<E>(Tok::RParen)?;
                    return Ok(Expr::Func {
                        name: fname,
                        distinct,
                        star,
                        args,
                    });
                }
                if let Some((lit, used)) = self.literal_at(self.pos) {
                    self.pos += used;
                    return Ok(Expr::Literal(lit));
                }
                // Column reference, possibly qualified. Parse the
                // tokens directly (name() would eat the qualifier).
                self.pos += 1;
                if self.cur().tok == Tok::Dot {
                    self.pos += 1;
                    match self.cur().tok {
                        Tok::Ident(h2, _) => {
                            self.pos += 1;
                            return Ok(Expr::Column {
                                table: Some(h),
                                name: h2,
                            });
                        }
                        _ => return Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
                    }
                }
                Ok(Expr::Column {
                    table: None,
                    name: h,
                })
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    // ---------- rich SELECT ----------

    fn select_rich<E>(&mut self) -> Result<SelectStmt, E> {
        self.expect_kw::<E>(b"SELECT")?;
        let distinct = if self.eat_kw(b"DISTINCT") {
            true
        } else {
            let _ = self.eat_kw(b"ALL");
            false
        };
        let mut items = AVec::new();
        loop {
            if self.eat(Tok::Star) {
                items.push(SelectItem::Star);
            } else if let Some((h, _, _)) = self.ident_at(self.pos)
                && self.toks.as_slice().get(self.pos + 1).map(|t| t.tok) == Some(Tok::Dot)
                && self.toks.as_slice().get(self.pos + 2).map(|t| t.tok) == Some(Tok::Star)
            {
                self.pos += 3;
                items.push(SelectItem::TableStar(h));
            } else {
                let start = self.cur().offset;
                let e = self.parse_expr::<E>()?;
                let end = self.cur().offset;
                let text = self
                    .arena
                    .alloc_copy_handle(trim(&self.sql[start..end.min(self.sql.len())]))?;
                let alias = self.opt_alias();
                items.push(SelectItem::Expr(e, alias, text));
            }
            if self.eat(Tok::Comma) {
                continue;
            }
            break;
        }
        let mut from = AVec::new();
        if self.eat_kw(b"FROM") {
            from.push(self.join_source::<E>(JoinKind::Inner, true)?);
            loop {
                if self.eat(Tok::Comma) {
                    from.push(self.join_source::<E>(JoinKind::Cross, false)?);
                    continue;
                }
                let kind = if self.eat_kw(b"JOIN") {
                    Some(JoinKind::Inner)
                } else if self.eat_kw(b"INNER") {
                    self.expect_kw::<E>(b"JOIN")?;
                    Some(JoinKind::Inner)
                } else if self.eat_kw(b"LEFT") {
                    let _ = self.eat_kw(b"OUTER");
                    self.expect_kw::<E>(b"JOIN")?;
                    Some(JoinKind::Left)
                } else if self.eat_kw(b"CROSS") {
                    self.expect_kw::<E>(b"JOIN")?;
                    Some(JoinKind::Cross)
                } else {
                    None
                };
                let Some(kind) = kind else { break };
                from.push(self.join_source::<E>(kind, false)?);
            }
        }
        let filter = if self.eat_kw(b"WHERE") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        let mut group_by = AVec::new();
        if self.eat_kw(b"GROUP") {
            self.expect_kw::<E>(b"BY")?;
            loop {
                group_by.push(self.parse_expr::<E>()?);
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        let having = if self.eat_kw(b"HAVING") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        let mut order_by = AVec::new();
        if self.eat_kw(b"ORDER") {
            self.expect_kw::<E>(b"BY")?;
            loop {
                let e = self.parse_expr::<E>()?;
                let desc = if self.eat_kw(b"DESC") {
                    true
                } else {
                    let _ = self.eat_kw(b"ASC");
                    false
                };
                // NULLS FIRST/LAST: SQLite's defaults (ASC: first,
                // DESC: last) are what the executor implements;
                // accept and ignore the explicit form when it
                // matches, which is the common case.
                if self.eat_kw(b"NULLS") {
                    let _ = self.eat_kw(b"FIRST") || self.eat_kw(b"LAST");
                }
                order_by.push((e, desc));
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        let mut limit = None;
        let mut offset = 0i64;
        if self.eat_kw(b"LIMIT") {
            let first = self.limit_int::<E>()?;
            if self.eat(Tok::Comma) {
                // LIMIT offset, count
                offset = first;
                limit = Some(self.limit_int::<E>()?);
            } else {
                limit = Some(first);
                if self.eat_kw(b"OFFSET") {
                    offset = self.limit_int::<E>()?;
                }
            }
        }
        Ok(SelectStmt {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    fn limit_int<E>(&mut self) -> Result<i64, E> {
        match self.literal_at(self.pos) {
            Some((Literal::Integer(v), used)) => {
                self.pos += used;
                Ok(v)
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnsupportedClause)),
        }
    }

    fn join_source<E>(&mut self, kind: JoinKind, first: bool) -> Result<FromSource, E> {
        let table = self.name::<E>()?;
        let alias = self.opt_alias();
        let on = if !first && self.eat_kw(b"ON") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        Ok(FromSource {
            table,
            alias,
            kind,
            on,
        })
    }

    // ---------- ALTER TABLE ADD COLUMN ----------

    fn alter_table<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"ALTER")?;
        self.expect_kw::<E>(b"TABLE")?;
        let table = self.name::<E>()?;
        if !self.eat_kw(b"ADD") {
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        let _ = self.eat_kw(b"COLUMN");
        // The rest of the statement is one column definition.
        let from = self.pos;
        let mut to = from;
        while to < self.limit {
            match self.toks.as_slice()[to].tok {
                Tok::Eof | Tok::Semi => break,
                _ => to += 1,
            }
        }
        if to == from {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        }
        let mut columns: AVec<ColumnDef> = AVec::new();
        let mut pk_names: AVec<ABytes> = AVec::new();
        let mut pk_desc: AVec<ABytes> = AVec::new();
        let mut sets: AVec<ConstraintSet> = AVec::new();
        self.create_item::<E>(
            from,
            to,
            &mut columns,
            &mut pk_names,
            &mut pk_desc,
            &mut sets,
        )?;
        if columns.len() != 1 {
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        let column = columns.as_slice()[0].clone();
        // Raw definition text (SQLite stores it verbatim into the
        // rewritten CREATE TABLE).
        let start = self.toks.as_slice()[from].offset;
        let end = if to < self.limit {
            self.toks.as_slice()[to].offset
        } else {
            self.sql.len()
        };
        let text = self
            .arena
            .alloc_copy_handle(trim(&self.sql[start..end.min(self.sql.len())]))?;
        self.pos = to;
        Ok(Ast::AlterAddColumn {
            table,
            column,
            text,
        })
    }

    // ---------- CREATE / DROP VIEW ----------

    fn create_view<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"CREATE")?;
        self.expect_kw::<E>(b"VIEW")?;
        let if_not_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"NOT")?;
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        let name = self.name::<E>()?;
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
        self.expect_kw::<E>(b"AS")?;
        let select = self.select_rich::<E>()?;
        Ok(Ast::CreateView {
            name,
            if_not_exists,
            columns,
            select,
        })
    }

    fn drop_view<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"DROP")?;
        self.expect_kw::<E>(b"VIEW")?;
        let if_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        let name = self.name::<E>()?;
        Ok(Ast::DropView { name, if_exists })
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

// ================= v0.5 grammar (feature = "v05") =================

#[cfg(feature = "v05")]
impl<'a> Parser<'a> {
    /// A bare column name with no qualifier: SQLite rejects
    /// `SET t.col = ...` as a syntax error (oracle-verified), so
    /// unlike `name()` a following `.` is an error, not a
    /// schema qualifier to discard.
    fn plain_name<E>(&mut self) -> Result<ABytes, E> {
        match self.cur().tok {
            Tok::Ident(h, _) => {
                self.pos += 1;
                if self.cur().tok == Tok::Dot {
                    return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
                }
                Ok(h)
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    fn update_stmt<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"UPDATE")?;
        let mut or_ignore = false;
        if self.eat_kw(b"OR") {
            if self.eat_kw(b"IGNORE") {
                or_ignore = true;
            } else if self.eat_kw(b"REPLACE") {
                // OR REPLACE deletes the conflicting row; a later
                // phase. Rejected by name rather than mis-parsed.
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            } else if !(self.eat_kw(b"ABORT") || self.eat_kw(b"FAIL") || self.eat_kw(b"ROLLBACK")) {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
            // OR ABORT / FAIL / ROLLBACK: the engine's default
            // (abort the statement) — nothing to record.
        }
        let table = self.name::<E>()?;
        // The alias is optional, but `SET` must not be eaten as
        // one (it is not in the clause-keyword stop list).
        let alias = if self.cur_is_kw(b"SET") {
            None
        } else {
            self.opt_alias()
        };
        self.expect_kw::<E>(b"SET")?;
        let mut assignments: AVec<(ABytes, Expr)> = AVec::new();
        loop {
            let col = self.plain_name::<E>()?;
            self.expect::<E>(Tok::Eq)?;
            let e = self.parse_expr::<E>()?;
            assignments.push((col, e));
            if self.eat(Tok::Comma) {
                continue;
            }
            break;
        }
        let filter = if self.eat_kw(b"WHERE") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        // UPDATE ... FROM, ORDER BY/LIMIT (the oracle build
        // rejects those too) and RETURNING are later phases;
        // parse()'s expect_end rejects any trailing input.
        Ok(Ast::Update {
            table,
            alias,
            or_ignore,
            assignments,
            filter,
        })
    }

    fn delete_stmt<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"DELETE")?;
        self.expect_kw::<E>(b"FROM")?;
        let table = self.name::<E>()?;
        let filter = if self.eat_kw(b"WHERE") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        Ok(Ast::Delete { table, filter })
    }

    fn drop_table<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"DROP")?;
        self.expect_kw::<E>(b"TABLE")?;
        let if_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        let name = self.name::<E>()?;
        Ok(Ast::DropTable { name, if_exists })
    }
}
