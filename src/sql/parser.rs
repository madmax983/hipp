//! Recursive-descent parser for the v0.1 grammar (SPEC §5.6).
//!
//! The `CREATE TABLE` path accepts the pin's whole column and
//! table constraint grammar — `sqlite_schema` rows written by real
//! SQLite must re-parse (§5.5) — and, since v0.16, nothing more:
//! every item is walked clause by clause, because whatever is
//! accepted is stored verbatim and must load on the pin. Foreign
//! keys, constraint-level ON CONFLICT and DEFERRABLE are parsed
//! and not enforced; CHECK, DEFAULT, generated columns, COLLATE,
//! NOT NULL, PRIMARY KEY and UNIQUE are (CHECK and generated
//! columns only with the v0.4 evaluator — the tiny profile refuses
//! them).

use crate::arena::{ABytes, AVec, Arena};
use crate::error::{Error, ParseKind, Result};
#[cfg(any(feature = "v04", feature = "indexes"))]
use crate::record::Coll;

#[cfg(feature = "v09")]
use super::ast::CompoundOp;
#[cfg(feature = "v04")]
use super::ast::SortOrder;
#[cfg(feature = "v07")]
use super::ast::UpsertAction;
use super::ast::{
    Ast, BinOp, ColumnDef, ConflictAction, ConstraintSet, Expr, FromSource, IndexColumnDef,
    JoinKind, Literal, SelectItem, SelectStmt, Upsert,
};
use alloc::boxed::Box;

use super::lexer::{self, Tok, Token};

const MAX_DEPTH: usize = 32;

// Expression binding powers (v0.16: the pin's precedence levels,
// loosest first; see `Parser::expr_bp`).
#[cfg(feature = "v04")]
const BP_OR: u8 = 1;
#[cfg(feature = "v04")]
const BP_AND: u8 = 2;
#[cfg(feature = "v04")]
const BP_NOT: u8 = 3;
#[cfg(feature = "v04")]
const BP_EQ: u8 = 4;
#[cfg(feature = "v04")]
const BP_REL: u8 = 5;
#[cfg(feature = "v16")]
const BP_BIT: u8 = 7;
#[cfg(feature = "v04")]
const BP_ADD: u8 = 8;
#[cfg(feature = "v04")]
const BP_MUL: u8 = 9;
#[cfg(feature = "v04")]
const BP_CONCAT: u8 = 10;
#[cfg(feature = "v04")]
const BP_COLLATE: u8 = 11;

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

/// What supplies an INSERT's rows (v0.8): VALUES rows, or a
/// SELECT source (exactly one of the two is meaningful —
/// `select` is `Some` iff the source was a SELECT, in which
/// case `rows` is empty).
struct InsertSource {
    rows: AVec<AVec<Expr>>,
    select: Option<Box<SelectStmt>>,
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

/// `parse`, also returning the statement's parameter numbering
/// (v0.16, feature `params`). A parameter inside a CREATE TABLE
/// (other than its AS SELECT), ALTER TABLE or CREATE VIEW is
/// refused, as on the pin: those definitions are stored as text
/// and re-read without bindings.
#[cfg(feature = "params")]
pub fn parse_params<E>(sql: &[u8], arena: &Arena) -> Result<(Ast, lexer::ParamTable), E> {
    let mut params = lexer::ParamTable::default();
    let toks = lexer::lex_params::<E>(sql, arena, &mut params)?;
    let mut p = Parser {
        limit: toks.len(),
        toks,
        pos: 0,
        arena,
        sql,
    };
    let ast = p.statement::<E>()?;
    p.expect_end::<E>()?;
    let stored = match &ast {
        #[cfg(feature = "v16")]
        Ast::CreateTable { as_select, .. } => as_select.is_none(),
        #[cfg(not(feature = "v16"))]
        Ast::CreateTable { .. } => true,
        Ast::AlterAddColumn { .. } | Ast::CreateView { .. } => true,
        _ => false,
    };
    if stored && params.count > 0 {
        return Err(Error::Schema {
            kind: crate::error::SchemaKind::ParameterNotAllowed,
        });
    }
    Ok((ast, params))
}

/// Parse one stand-alone expression (v0.16): the stored text of
/// a CHECK constraint, generated column or DEFAULT, re-parsed by
/// the planner into `arena`. The whole input must be the one
/// expression.
#[cfg(feature = "v04")]
pub fn parse_expr_text<E>(sql: &[u8], arena: &Arena) -> Result<Expr, E> {
    let toks = lexer::lex::<E>(sql, arena)?;
    let mut p = Parser {
        limit: toks.len(),
        toks,
        pos: 0,
        arena,
        sql,
    };
    let e = p.parse_expr::<E>()?;
    if !p.at_eof() {
        return Err(perr(p.cur().offset, ParseKind::TrailingInput));
    }
    Ok(e)
}

/// Where the stored schema text of a CREATE TABLE / INDEX / VIEW
/// statement starts (v0.16): the pin rebuilds that text as
/// `CREATE TABLE ` (`CREATE VIEW `, `CREATE [UNIQUE] INDEX `)
/// followed by the statement from the bare created name on, so a
/// `main.` qualifier, `IF NOT EXISTS` and the keywords' spelling
/// are not kept. Returns that prefix and the name's byte offset;
/// `None` for text that does not lex.
///
/// It also refuses AUTOINCREMENT (`Error::Unsupported`, v0.16):
/// hipp does not maintain the companion `sqlite_sequence` table,
/// and a file holding an AUTOINCREMENT table without it is
/// "malformed" to the pin on its next INSERT. (Existing files that
/// have one still load: the catalog parses, it does not create.)
/// The word is reserved, so an unquoted occurrence is the clause.
pub fn schema_text_start<E>(
    sql: &[u8],
    arena: &Arena,
) -> Result<Option<(&'static [u8], usize)>, E> {
    let Ok(toks) = lexer::lex::<E>(sql, arena) else {
        return Ok(None);
    };
    let p = Parser {
        limit: toks.len(),
        toks,
        pos: 0,
        arena,
        sql,
    };
    if (0..p.limit).any(|i| p.is_kw(i, b"AUTOINCREMENT")) {
        return Err(Error::Unsupported);
    }
    // CREATE [UNIQUE] TABLE|INDEX|VIEW [IF NOT EXISTS] [main .] name
    let unique = p.is_kw(1, b"UNIQUE");
    let kind = 1 + usize::from(unique);
    let prefix: &'static [u8] = if p.is_kw(kind, b"VIEW") {
        b"CREATE VIEW "
    } else if p.is_kw(kind, b"INDEX") {
        if unique {
            b"CREATE UNIQUE INDEX "
        } else {
            b"CREATE INDEX "
        }
    } else {
        b"CREATE TABLE "
    };
    let mut i = kind + 1;
    if p.is_kw(i, b"IF") {
        i += 3;
    }
    let t = p.toks.as_slice();
    if t.get(i + 1).is_some_and(|d| matches!(d.tok, Tok::Dot)) {
        i += 2;
    }
    Ok(t.get(i).map(|n| (prefix, n.offset)))
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
        // Discriminant compare: `eat` / `expect` only take
        // payload-free tokens, and it keeps the derived
        // `PartialEq for Tok` (f64 / handle payloads) out of the
        // tiny build.
        if core::mem::discriminant(&self.cur().tok) == core::mem::discriminant(&t) {
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

    /// The collating sequence named by token `i` (v0.16): an
    /// identifier or string (`COLLATE nocase`, `COLLATE
    /// "NoCase"`, `COLLATE 'rtrim'`), matched case-insensitively.
    /// An unknown name is the pin's "no such collation
    /// sequence". Not built for the tiny profile, which has no
    /// comparisons a collation could change (it skips the
    /// clause, as before v0.16).
    #[cfg(any(feature = "v04", feature = "indexes"))]
    fn coll_at<E>(&self, i: usize) -> Result<Coll, E> {
        let t = self
            .toks
            .as_slice()
            .get(i.min(self.limit - 1))
            .map(|t| t.tok);
        match t {
            Some(Tok::Ident(h, _)) | Some(Tok::Text(h)) => Coll::named(handle_bytes(self.arena, h))
                .ok_or(Error::Schema {
                    kind: crate::error::SchemaKind::UnknownCollation,
                }),
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    /// A table/column name: any identifier token (quoted or not).
    fn name<E>(&mut self) -> Result<ABytes, E> {
        match self.cur().tok {
            Tok::Ident(h, _) => {
                self.pos += 1;
                // Accept (and discard) the schema qualifier `main.`
                // — the only schema a hipp connection has. Any
                // other (`temp.t`, an attached name) is refused:
                // hipp used to drop it and use `main.t`, where the
                // pin finds no such table (or, for CREATE TABLE
                // temp.t, makes a temporary one outside the file).
                if matches!(self.cur().tok, Tok::Dot) {
                    if !eq_ci(handle_bytes(self.arena, h), b"main") {
                        return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
                    }
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
            self.insert::<E>(false)
        } else if self.cur_is_kw(b"REPLACE") {
            self.replace_dispatch::<E>()
        } else if self.cur_is_kw(b"UPDATE") {
            self.update_dispatch::<E>()
        } else if self.cur_is_kw(b"DELETE") {
            self.delete_dispatch::<E>()
        } else if self.cur_is_kw(b"SELECT") {
            self.select::<E>()
        } else if self.cur_is_kw(b"VALUES") {
            // A standalone VALUES statement (v0.9): a select
            // statement whose first core is a VALUES core.
            #[cfg(feature = "v09")]
            {
                Ok(Ast::Select(self.select_rich::<E>()?))
            }
            #[cfg(not(feature = "v09"))]
            {
                Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
            }
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
        // The indexed table is a bare name on the pin (`ON
        // main.t` is a syntax error there — and stored, a schema
        // it cannot load).
        if matches!(
            self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
            Some(Tok::Dot)
        ) {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        }
        let table = self.name::<E>()?;
        self.expect::<E>(Tok::LParen)?;
        let mut columns: AVec<IndexColumnDef> = AVec::new();
        loop {
            let col = self.name::<E>()?;
            // An expression index column looks like `abs(a)` or
            // `(a+1)`: a '(' right here (or as the item start,
            // caught because name() rejects '(') is outside v0.3.
            if matches!(self.cur().tok, Tok::LParen) {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
            // v0.16: an explicit key collation (BINARY, NOCASE
            // or RTRIM — the index orders and finds keys by it).
            let mut coll = None;
            if self.eat_kw(b"COLLATE") {
                coll = Some(self.coll_at::<E>(self.pos)?);
                self.pos += 1;
            }
            let desc = if self.eat_kw(b"DESC") {
                true
            } else {
                let _ = self.eat_kw(b"ASC");
                false
            };
            columns.push(IndexColumnDef {
                name: col,
                desc,
                coll,
            });
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
        // CREATE TABLE ... AS SELECT (v0.16, `v16`): the columns
        // come from the SELECT at plan time.
        #[cfg(feature = "v16")]
        if self.eat_kw(b"AS") {
            let select = self.select_rich::<E>()?;
            return Ok(Ast::CreateTable {
                name,
                if_not_exists,
                columns: AVec::new(),
                pk_columns: AVec::new(),
                constraint_sets: AVec::new(),
                checks: AVec::new(),
                strict: false,
                as_select: Some(Box::new(select)),
            });
        }
        self.expect::<E>(Tok::LParen)?;
        // Split the parenthesised body into top-level comma items.
        let items = self.split_items::<E>()?;
        let mut columns: AVec<ColumnDef> = AVec::new();
        let mut pk_names: AVec<ABytes> = AVec::new();
        let mut pk_desc_names: AVec<ABytes> = AVec::new();
        let mut sets: AVec<ConstraintSet> = AVec::new();
        #[cfg(feature = "v04")]
        let mut checks: AVec<ABytes> = AVec::new();
        // Column definitions first, then table constraints (the
        // pin's `columnlist conslist_opt`): a column after a table
        // constraint is a syntax error.
        let mut in_cons = false;
        for (from, to) in items.iter() {
            let ncols = columns.len();
            in_cons |= self.create_item::<E>(
                *from,
                *to,
                &mut columns,
                &mut pk_names,
                &mut pk_desc_names,
                &mut sets,
                #[cfg(feature = "v04")]
                &mut checks,
            )?;
            if in_cons && columns.len() != ncols {
                return Err(perr(
                    self.item_end_offset(*from),
                    ParseKind::UnexpectedToken,
                ));
            }
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
        #[cfg(feature = "indexes")]
        let sets = self.dedupe_sets(sets, &columns, &pk_columns);
        // Table options (v0.16, `v16`): a comma-separated list of
        // STRICT and WITHOUT ROWID, as on the pin (`STRICT,STRICT`
        // parses; `STRICT STRICT` does not). WITHOUT ROWID stays
        // outside every hipp phase and is refused.
        #[cfg(feature = "v16")]
        let mut strict = false;
        #[cfg(feature = "v16")]
        if self.cur_is_kw(b"STRICT") || self.cur_is_kw(b"WITHOUT") {
            loop {
                if self.cur_is_kw(b"WITHOUT") {
                    return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
                }
                self.expect_kw::<E>(b"STRICT")?;
                strict = true;
                if !self.eat(Tok::Comma) {
                    break;
                }
            }
        }
        // Trailing clauses: WITHOUT ROWID is outside v0.1.
        #[cfg(not(feature = "v16"))]
        if self.cur_is_kw(b"WITHOUT") {
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        Ok(Ast::CreateTable {
            name,
            if_not_exists,
            columns,
            pk_columns,
            constraint_sets: sets,
            #[cfg(feature = "v04")]
            checks,
            #[cfg(feature = "v16")]
            strict,
            #[cfg(feature = "v16")]
            as_select: None,
        })
    }

    /// Drop each PRIMARY KEY / UNIQUE set that repeats an earlier
    /// one — the same columns in the same order with the same
    /// collations, sort order aside — as the pin's
    /// `sqlite3CreateIndex` does for automatic indexes (v0.16:
    /// `a PRIMARY KEY UNIQUE` used to get two autoindexes, a
    /// schema the pin rejects as "orphan index"). A repeat of a
    /// UNIQUE set by the PRIMARY KEY makes the earlier set the
    /// key's. The rowid-alias key (no index) never merges.
    #[cfg(feature = "indexes")]
    fn dedupe_sets(
        &self,
        sets: AVec<ConstraintSet>,
        columns: &AVec<ColumnDef>,
        pk_columns: &AVec<ABytes>,
    ) -> AVec<ConstraintSet> {
        let name = |h: ABytes| handle_bytes(self.arena, h);
        let col = |h: ABytes| columns.iter().find(|c| eq_ci(name(c.name), name(h)));
        let alias = pk_columns.len() == 1
            && pk_columns
                .as_slice()
                .first()
                .and_then(|h| col(*h))
                .is_some_and(|c| {
                    !c.pk_desc && eq_ci(trim(handle_bytes(self.arena, c.decl)), b"INTEGER")
                });
        let coll = |d: &IndexColumnDef| d.coll.or_else(|| col(d.name).and_then(|c| c.coll));
        let mut out: AVec<ConstraintSet> = AVec::new();
        for s in sets.iter() {
            let indexed = |x: &ConstraintSet| !(x.primary_key && alias);
            let same = |k: &ConstraintSet| {
                k.columns.len() == s.columns.len()
                    && k.columns.iter().zip(s.columns.iter()).all(|(a, b)| {
                        eq_ci(name(a.name), name(b.name))
                            && coll(a).unwrap_or_default() == coll(b).unwrap_or_default()
                    })
            };
            if indexed(s)
                && let Some(k) = out
                    .as_mut_slice()
                    .iter_mut()
                    .find(|k| indexed(k) && same(k))
            {
                k.primary_key |= s.primary_key;
                continue;
            }
            out.push(s.clone());
        }
        out
    }

    /// The index of the `)` matching the `(` at token `k`,
    /// within the item ending at token `to` (v0.16).
    fn close_paren<E>(&self, k: usize, to: usize) -> Result<usize, E> {
        let mut depth = 0usize;
        let mut j = k;
        while j < to {
            match self.toks.as_slice()[j].tok {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Ok(j);
                    }
                }
                _ => {}
            }
            j += 1;
        }
        Err(perr(self.item_end_offset(to), ParseKind::UnexpectedToken))
    }

    /// The SQL text inside the parenthesised group opening at
    /// token `k` (v0.16: CHECK and generated-column bodies),
    /// plus the index of its closing `)`.
    #[cfg(feature = "v04")]
    fn paren_text<E>(&self, k: usize, to: usize) -> Result<(usize, ABytes), E> {
        if k >= to || !matches!(self.toks.as_slice()[k].tok, Tok::LParen) {
            return Err(perr(self.item_end_offset(k), ParseKind::UnexpectedToken));
        }
        let close = self.close_paren::<E>(k, to)?;
        let start = self.toks.as_slice()[k + 1].offset;
        let end = self.toks.as_slice()[close].offset;
        let text = trim(&self.sql[start.min(end)..end.min(self.sql.len())]);
        Ok((close, self.arena.alloc_copy_handle(text)?))
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

    /// Token `i` is the integer literal 9223372036854775808
    /// (leading zeros allowed) — lexed as a REAL, the one integer
    /// text a minus sign turns back into an INTEGER (v0.16).
    fn min_magnitude(&self, i: usize) -> bool {
        let Some(t) = self.toks.as_slice().get(i) else {
            return false;
        };
        if !matches!(t.tok, Tok::Real(f) if f == 9_223_372_036_854_775_808.0) {
            return false;
        }
        let z = self.sql.get(t.offset..).unwrap_or(&[]);
        // Without the evaluator there are no `_` separators.
        #[cfg(not(feature = "v04"))]
        {
            let mut z = z;
            while let [b'0', rest @ ..] = z {
                z = rest;
            }
            let d = b"9223372036854775808";
            z.starts_with(d)
                && !z
                    .get(d.len())
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_')
        }
        // The token's digits, without leading zeros and `_`
        // separators (v0.16: `-9_223_372_036_854_775_808` is
        // the INTEGER too).
        #[cfg(feature = "v04")]
        {
            let end = z
                .iter()
                .position(|b| !(b.is_ascii_digit() || *b == b'_'))
                .unwrap_or(z.len());
            if z.get(end)
                .is_some_and(|b| *b == b'.' || *b == b'e' || *b == b'E')
            {
                return false;
            }
            let mut digits = z
                .get(..end)
                .unwrap_or(&[])
                .iter()
                .filter(|b| **b != b'_')
                .skip_while(|b| **b == b'0');
            let d = b"9223372036854775808";
            d.iter().all(|c| digits.next() == Some(c)) && digits.next().is_none()
        }
    }

    /// Token `i` is a hexadecimal literal whose value is i64::MIN
    /// (`0x8000000000000000`, leading zeros allowed) — the one
    /// hex literal a minus sign cannot negate (v0.16).
    #[cfg(feature = "v04")]
    fn hex_min(&self, i: usize) -> bool {
        match self.toks.as_slice().get(i) {
            Some(t) if matches!(t.tok, Tok::Integer(i64::MIN)) => {
                matches!(
                    self.sql.get(t.offset..t.offset.saturating_add(2)),
                    Some(b"0x" | b"0X")
                )
            }
            _ => false,
        }
    }

    /// A parameter token at the cursor, consumed (v0.16,
    /// `params`): the literal positions of the tiny grammar.
    #[cfg(all(feature = "params", not(feature = "v04")))]
    fn param_here(&mut self) -> Option<Expr> {
        match self.cur().tok {
            Tok::Param(i, h) => {
                self.pos += 1;
                Some(Expr::Param(i, h))
            }
            _ => None,
        }
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
            Tok::Minus => {
                // The pin's `codeInteger`: `-` over the integer
                // literal 9223372036854775808 (unary `+` between
                // is transparent) is INTEGER i64::MIN, not a REAL
                // (v0.16); `-` over i64::MIN itself overflows to
                // REAL.
                let mut j = i + 1;
                while matches!(self.toks.as_slice().get(j).map(|t| t.tok), Some(Tok::Plus)) {
                    j += 1;
                }
                if self.min_magnitude(j) {
                    return Some((Literal::Integer(i64::MIN), j - i + 1));
                }
                // `-0x8000000000000000` is the pin's "hex literal
                // too big", not a REAL (v0.16 fix): no literal
                // here, so the evaluator's unary minus reports
                // it. (The tiny lexer refuses the literal itself.)
                #[cfg(feature = "v04")]
                if self.hex_min(j) {
                    return None;
                }
                match self.literal_at(i + 1) {
                    Some((Literal::Integer(v), n)) => Some((
                        v.checked_neg()
                            .map_or(Literal::Real(-(v as f64)), Literal::Integer),
                        n + 1,
                    )),
                    Some((Literal::Real(f), n)) => Some((Literal::Real(-f), n + 1)),
                    _ => None,
                }
            }
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

    // ---------------- v0.7 statement shim ----------------
    // The standalone `REPLACE` verb (== INSERT OR REPLACE) exists
    // only under the `v07` feature.

    #[cfg(feature = "v07")]
    fn replace_dispatch<E>(&mut self) -> Result<Ast, E> {
        self.insert::<E>(true)
    }

    #[cfg(not(feature = "v07"))]
    fn replace_dispatch<E>(&mut self) -> Result<Ast, E> {
        Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
    }

    // ---------------- INSERT ----------------

    fn insert<E>(&mut self, verb_replace: bool) -> Result<Ast, E> {
        let conflict = if verb_replace {
            self.expect_kw::<E>(b"REPLACE")?;
            ConflictAction::Replace
        } else {
            self.expect_kw::<E>(b"INSERT")?;
            #[cfg(feature = "v07")]
            let c = self.conflict_clause::<E>()?;
            #[cfg(not(feature = "v07"))]
            let c = ConflictAction::Abort;
            c
        };
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
        // `INSERT INTO t DEFAULT VALUES` (v0.16, `v16`): one row
        // of defaults — an empty target list with one empty VALUES
        // row. The pin allows no column list before it and no
        // ON CONFLICT after it (RETURNING is fine).
        #[cfg(feature = "v16")]
        if self.cur_is_kw(b"DEFAULT") {
            self.pos += 1;
            if columns.is_some() {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
            self.expect_kw::<E>(b"VALUES")?;
            let mut rows = AVec::new();
            rows.push(AVec::new());
            #[cfg(feature = "v07")]
            let returning = self.returning_clause::<E>()?;
            #[cfg(not(feature = "v07"))]
            let returning = AVec::new();
            return Ok(Ast::Insert {
                table,
                columns: Some(AVec::new()),
                rows,
                select: None,
                conflict,
                upsert: None,
                returning,
            });
        }
        let InsertSource { rows, select } = self.insert_source::<E>()?;
        let (upsert, returning) = self.insert_tail::<E>()?;
        Ok(Ast::Insert {
            table,
            columns,
            rows,
            select,
            conflict,
            upsert,
            returning,
        })
    }

    /// An INSERT's row source (v0.8): `VALUES (...)` rows, or —
    /// under `v08` — a SELECT whose result rows are inserted.
    /// Without the feature a SELECT source is not consumed here
    /// and the statement fails at `expect_kw(VALUES)` exactly as
    /// before.
    #[cfg(feature = "v08")]
    fn insert_source<E>(&mut self) -> Result<InsertSource, E> {
        if self.cur_is_kw(b"SELECT") {
            return Ok(InsertSource {
                rows: AVec::new(),
                select: Some(Box::new(self.select_rich::<E>()?)),
            });
        }
        Ok(InsertSource {
            rows: self.values_rows::<E>()?,
            select: None,
        })
    }

    #[cfg(not(feature = "v08"))]
    fn insert_source<E>(&mut self) -> Result<InsertSource, E> {
        Ok(InsertSource {
            rows: self.values_rows::<E>()?,
            select: None,
        })
    }

    /// The `VALUES` row list: v0.2 one or more parenthesised
    /// rows, comma-separated; v0.4 each value is an expression
    /// (a plain literal under the pre-v0.4 grammar).
    fn values_rows<E>(&mut self) -> Result<AVec<AVec<Expr>>, E> {
        self.expect_kw::<E>(b"VALUES")?;
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
        Ok(rows)
    }

    /// The optional `OR <action>` after INSERT / UPDATE (v0.7):
    /// absent is `Abort` (the engine default).
    #[cfg(feature = "v07")]
    fn conflict_clause<E>(&mut self) -> Result<ConflictAction, E> {
        if !self.eat_kw(b"OR") {
            return Ok(ConflictAction::Abort);
        }
        if self.eat_kw(b"IGNORE") {
            Ok(ConflictAction::Ignore)
        } else if self.eat_kw(b"REPLACE") {
            Ok(ConflictAction::Replace)
        } else if self.eat_kw(b"ABORT") || self.eat_kw(b"FAIL") || self.eat_kw(b"ROLLBACK") {
            Ok(ConflictAction::Abort)
        } else {
            Err(perr(self.cur().offset, ParseKind::UnexpectedToken))
        }
    }

    /// What may follow an INSERT's VALUES rows (v0.7): an
    /// optional `ON CONFLICT` upsert clause, then an optional
    /// `RETURNING` clause. Without the `v07` feature nothing may
    /// follow (the caller's `expect_end` rejects trailing input).
    #[cfg(feature = "v07")]
    fn insert_tail<E>(&mut self) -> Result<(Option<Box<Upsert>>, AVec<SelectItem>), E> {
        let upsert = if self.eat_kw(b"ON") {
            Some(Box::new(self.upsert_clause::<E>()?))
        } else {
            None
        };
        let returning = self.returning_clause::<E>()?;
        Ok((upsert, returning))
    }

    #[cfg(not(feature = "v07"))]
    fn insert_tail<E>(&mut self) -> Result<(Option<Box<Upsert>>, AVec<SelectItem>), E> {
        Ok((None, AVec::new()))
    }

    /// `CONFLICT [(col, ...)] [WHERE expr] DO NOTHING | DO
    /// UPDATE SET ... [WHERE expr]` — the leading `ON` is eaten
    /// by the caller (v0.7).
    #[cfg(feature = "v07")]
    fn upsert_clause<E>(&mut self) -> Result<Upsert, E> {
        self.expect_kw::<E>(b"CONFLICT")?;
        let mut target = AVec::new();
        let mut target_colls = AVec::new();
        if self.eat(Tok::LParen) {
            loop {
                target.push(self.name::<E>()?);
                // Indexed-column decorations SQLite permits in a
                // conflict target: COLLATE <name>, ASC / DESC
                // (v0.16: the collation must match the index's).
                let mut coll = None;
                if self.eat_kw(b"COLLATE") {
                    coll = Some(self.coll_at::<E>(self.pos)?);
                    self.pos += 1;
                }
                target_colls.push(coll);
                let _ = self.eat_kw(b"ASC") || self.eat_kw(b"DESC");
                if self.eat(Tok::Comma) {
                    continue;
                }
                self.expect::<E>(Tok::RParen)?;
                break;
            }
        }
        let target_filter = if self.eat_kw(b"WHERE") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        self.expect_kw::<E>(b"DO")?;
        let action = if self.eat_kw(b"NOTHING") {
            UpsertAction::Nothing
        } else {
            self.expect_kw::<E>(b"UPDATE")?;
            self.expect_kw::<E>(b"SET")?;
            let assignments = self.set_list::<E>()?;
            let filter = if self.eat_kw(b"WHERE") {
                Some(self.parse_expr::<E>()?)
            } else {
                None
            };
            UpsertAction::Update {
                assignments,
                filter,
            }
        };
        Ok(Upsert {
            target,
            target_colls,
            target_filter,
            action,
        })
    }

    /// An optional `RETURNING <items>` clause (v0.7), shared by
    /// INSERT / UPDATE / DELETE: SELECT-shaped result items
    /// (`*`, `t.*`, or an expression with an optional alias).
    /// Returns an empty vec when the clause is absent.
    #[cfg(feature = "v07")]
    fn returning_clause<E>(&mut self) -> Result<AVec<SelectItem>, E> {
        let mut items = AVec::new();
        if !self.eat_kw(b"RETURNING") {
            return Ok(items);
        }
        loop {
            if self.eat(Tok::Star) {
                items.push(SelectItem::Star);
            } else if let Some((h, _, _)) = self.ident_at(self.pos)
                && matches!(
                    self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
                    Some(Tok::Dot)
                )
                && matches!(
                    self.toks.as_slice().get(self.pos + 2).map(|t| t.tok),
                    Some(Tok::Star)
                )
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
        Ok(items)
    }

    /// One INSERT value: a full expression under `v04`, a plain
    /// literal otherwise.
    #[cfg(feature = "v04")]
    fn value_expr<E>(&mut self) -> Result<Expr, E> {
        self.parse_expr::<E>()
    }

    #[cfg(not(feature = "v04"))]
    fn value_expr<E>(&mut self) -> Result<Expr, E> {
        #[cfg(feature = "params")]
        if let Some(e) = self.param_here() {
            return Ok(e);
        }
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
            derived: None,
            #[cfg(feature = "v04")]
            natural: false,
            #[cfg(feature = "v04")]
            using: AVec::new(),
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
            #[cfg(feature = "params")]
            let rhs = match self.param_here() {
                Some(e) => e,
                None => {
                    let (lit, used) = self
                        .literal_at(self.pos)
                        .ok_or(perr(self.cur().offset, ParseKind::UnsupportedClause))?;
                    self.pos += used;
                    Expr::Literal(lit)
                }
            };
            #[cfg(not(feature = "params"))]
            let rhs = {
                let (lit, used) = self
                    .literal_at(self.pos)
                    .ok_or(perr(self.cur().offset, ParseKind::UnsupportedClause))?;
                self.pos += used;
                Expr::Literal(lit)
            };
            filter = Some(Expr::Binary(
                BinOp::Eq,
                Box::new(Expr::Column {
                    table: None,
                    name: col,
                }),
                Box::new(rhs),
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
            offset: None,
            compound: AVec::new(),
            values: AVec::new(),
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
            // The pin's remaining join keywords (JOIN_KW tokens,
            // never a bare alias there): eating them as an alias
            // turned `a RIGHT JOIN b` into an inner join (v0.16).
            b"NATURAL",
            b"RIGHT",
            b"FULL",
            b"ON",
            b"USING",
            b"UNION",
            b"EXCEPT",
            b"INTERSECT",
        ];
        match self.ident_at(self.pos) {
            Some((h, false, _)) => {
                let b = handle_bytes(self.arena, h);
                // RETURNING (v0.16 fix, v07 grammar) ends an
                // UPDATE ... FROM source list: `FROM s RETURNING v`
                // used to alias `s` as RETURNING.
                KWS.iter().any(|k| eq_ci(b, k)) || (cfg!(feature = "v07") && eq_ci(b, b"RETURNING"))
            }
            _ => false,
        }
    }

    /// An optional `[AS] name` alias: a quoted identifier, or a
    /// bare identifier that is not a clause keyword. Under `v16`
    /// a string literal works too, with or without AS (the pin's
    /// `as ::= AS nm | ids`, where both admit a STRING token):
    /// `SELECT a AS 'q'`, `FROM t 'x'`.
    fn opt_alias(&mut self) -> Option<ABytes> {
        #[cfg(feature = "v16")]
        {
            let at = if self.cur_is_kw(b"AS") { 1 } else { 0 };
            if let Some(Tok::Text(h)) = self.toks.as_slice().get(self.pos + at).map(|t| t.tok) {
                self.pos += at + 1;
                return Some(h);
            }
        }
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
    // The pin's `parse.y` precedence, loosest first:
    //   OR < AND < NOT < (= == != <> IS IN LIKE GLOB BETWEEN
    //   ISNULL NOTNULL NOT NULL) < (< <= > >=) < (& | << >>)
    //   < (+ -) < (* / %) < || < COLLATE < unary (- + ~).
    // A precedence-climbing loop reproduces the LALR parse:
    // binary operators associate to the left, and a postfix
    // operator (ISNULL, IN (...), COLLATE, ...) yields an
    // operand that tighter operators may still extend — `x
    // ISNULL + 1` is `(x ISNULL) + 1` on the pin. (v0.16 fix,
    // ungated: before it `<` and `=` shared one level, so `3 =
    // 2 < 3` read `(3 = 2) < 3`.)

    pub(crate) fn parse_expr<E>(&mut self) -> Result<Expr, E> {
        self.expr_bp::<E>(BP_OR)
    }

    /// An expression whose operators all bind at least as
    /// tightly as `min`.
    fn expr_bp<E>(&mut self, min: u8) -> Result<Expr, E> {
        let mut l = self.expr_prefix::<E>()?;
        while let Some(bp) = self.infix_bp() {
            if bp < min {
                break;
            }
            l = self.expr_infix::<E>(l, bp)?;
        }
        Ok(l)
    }

    /// The binding power of the operator at the cursor, if it
    /// is one that continues an expression.
    fn infix_bp(&self) -> Option<u8> {
        Some(match self.cur().tok {
            Tok::Eq | Tok::Ne => BP_EQ,
            Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge => BP_REL,
            #[cfg(feature = "v16")]
            Tok::BitAnd | Tok::BitOr | Tok::Shl | Tok::Shr => BP_BIT,
            Tok::Plus | Tok::Minus => BP_ADD,
            Tok::Star | Tok::Slash | Tok::Percent => BP_MUL,
            Tok::Concat => BP_CONCAT,
            Tok::Ident(h, false) => {
                let b = handle_bytes(self.arena, h);
                if eq_ci(b, b"OR") {
                    BP_OR
                } else if eq_ci(b, b"AND") {
                    BP_AND
                } else if eq_ci(b, b"NOT") {
                    // Only as the head of a negated operator: an
                    // expression followed by any other NOT ends.
                    let n = self.pos + 1;
                    if self.is_kw(n, b"IN")
                        || self.is_kw(n, b"LIKE")
                        || self.is_kw(n, b"BETWEEN")
                        || (cfg!(feature = "v16")
                            && (self.is_kw(n, b"GLOB") || self.is_kw(n, b"NULL")))
                    {
                        BP_EQ
                    } else {
                        return None;
                    }
                } else if [
                    &b"IS"[..],
                    b"ISNULL",
                    b"NOTNULL",
                    b"IN",
                    b"LIKE",
                    b"BETWEEN",
                ]
                .iter()
                .any(|k| eq_ci(b, k))
                    || (cfg!(feature = "v16") && eq_ci(b, b"GLOB"))
                {
                    BP_EQ
                } else if cfg!(feature = "v16") && eq_ci(b, b"COLLATE") {
                    BP_COLLATE
                } else {
                    return None;
                }
            }
            _ => return None,
        })
    }

    /// Apply the operator at the cursor (of binding power `bp`)
    /// to the left operand `l`.
    fn expr_infix<E>(&mut self, l: Expr, bp: u8) -> Result<Expr, E> {
        let bin = |op: BinOp, l: Expr, r: Expr| Expr::Binary(op, Box::new(l), Box::new(r));
        if bp == BP_OR || bp == BP_AND {
            self.pos += 1;
            let r = self.expr_bp::<E>(bp + 1)?;
            return Ok(bin(if bp == BP_OR { BinOp::Or } else { BinOp::And }, l, r));
        }
        if bp == BP_EQ && matches!(self.cur().tok, Tok::Ident(..)) {
            return self.expr_eq_kw::<E>(l);
        }
        #[cfg(feature = "v16")]
        if bp == BP_COLLATE {
            self.pos += 1;
            let c = self.coll_at::<E>(self.pos)?;
            self.pos += 1;
            return Ok(Expr::Collate(Box::new(l), c));
        }
        let op = match self.cur().tok {
            Tok::Eq => BinOp::Eq,
            Tok::Ne => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::Le => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::Ge => BinOp::Ge,
            #[cfg(feature = "v16")]
            Tok::BitAnd => BinOp::BitAnd,
            #[cfg(feature = "v16")]
            Tok::BitOr => BinOp::BitOr,
            #[cfg(feature = "v16")]
            Tok::Shl => BinOp::Shl,
            #[cfg(feature = "v16")]
            Tok::Shr => BinOp::Shr,
            Tok::Plus => BinOp::Add,
            Tok::Minus => BinOp::Sub,
            Tok::Star => BinOp::Mul,
            Tok::Slash => BinOp::Div,
            Tok::Percent => BinOp::Mod,
            _ => BinOp::Concat,
        };
        self.pos += 1;
        // `==` is spelled with two Eq tokens by the lexer.
        if op == BinOp::Eq {
            let _ = self.eat(Tok::Eq);
        }
        let r = self.expr_bp::<E>(bp + 1)?;
        Ok(bin(op, l, r))
    }

    /// The keyword operators of the `=` level: IS [NOT]
    /// [DISTINCT FROM], ISNULL, NOTNULL, NOT NULL, [NOT] IN /
    /// LIKE / GLOB / BETWEEN.
    fn expr_eq_kw<E>(&mut self, l: Expr) -> Result<Expr, E> {
        // Postfix ISNULL / NOTNULL.
        if self.eat_kw(b"ISNULL") {
            return Ok(Expr::IsNull(Box::new(l), false));
        }
        if self.eat_kw(b"NOTNULL") {
            return Ok(Expr::IsNull(Box::new(l), true));
        }
        if self.eat_kw(b"IS") {
            let neg = self.eat_kw(b"NOT");
            // `IS [NOT] DISTINCT FROM` (v0.16, `v16`): `IS NOT
            // DISTINCT FROM` is IS, `IS DISTINCT FROM` is IS NOT.
            #[cfg(feature = "v16")]
            let neg = if self.eat_kw(b"DISTINCT") {
                self.expect_kw::<E>(b"FROM")?;
                !neg
            } else {
                neg
            };
            let start = self.pos;
            let r = self.expr_bp::<E>(BP_REL)?;
            // `x IS [NOT] TRUE / FALSE` with the bare keyword as
            // the whole right operand is the pin's truth test
            // (`TK_TRUTH`): any true value IS TRUE, NULL is
            // neither (v0.16 fix, ungated: `2 IS TRUE` was 0).
            // Written as the CASE it equals.
            if self.pos == start + 1
                && let Expr::Literal(Literal::Integer(v @ (0 | 1))) = r
                && (self.is_kw(start, b"TRUE") || self.is_kw(start, b"FALSE"))
            {
                let test = if v == 1 { l } else { Expr::Not(Box::new(l)) };
                let (yes, no) = if neg { (0, 1) } else { (1, 0) };
                let mut whens = AVec::new();
                whens.push((test, Expr::Literal(Literal::Integer(yes))));
                return Ok(Expr::Case {
                    base: None,
                    whens,
                    otherwise: Some(Box::new(Expr::Literal(Literal::Integer(no)))),
                });
            }
            let op = if neg { BinOp::IsNot } else { BinOp::Is };
            return Ok(Expr::Binary(op, Box::new(l), Box::new(r)));
        }
        let neg = self.eat_kw(b"NOT");
        // Postfix `NOT NULL` (v0.16, `v16`): NOTNULL.
        #[cfg(feature = "v16")]
        if neg && self.cur_is_kw(b"NULL") {
            self.pos += 1;
            return Ok(Expr::IsNull(Box::new(l), true));
        }
        if self.eat_kw(b"IN") {
            return self.expr_in::<E>(l, neg);
        }
        if self.eat_kw(b"LIKE") {
            let r = self.expr_bp::<E>(BP_REL)?;
            // `x LIKE p ESCAPE e` (v0.16, `v16`) is the pin's
            // `like(p, x, e)` call, negated for NOT LIKE.
            #[cfg(feature = "v16")]
            if self.eat_kw(b"ESCAPE") {
                let esc = self.expr_bp::<E>(BP_REL)?;
                return self.pattern_call::<E>(b"like", r, l, Some(esc), neg);
            }
            return Ok(Expr::Like(Box::new(l), Box::new(r), neg));
        }
        // `x [NOT] GLOB p` (v0.16, `v16`): the pin's
        // `glob(p, x)` call (no ESCAPE form).
        #[cfg(feature = "v16")]
        if self.eat_kw(b"GLOB") {
            let r = self.expr_bp::<E>(BP_REL)?;
            return self.pattern_call::<E>(b"glob", r, l, None, neg);
        }
        if self.eat_kw(b"BETWEEN") {
            // The lower bound runs to the BETWEEN's own AND; the
            // upper bound takes only operators tighter than `=`.
            let lo = self.expr_bp::<E>(BP_NOT)?;
            self.expect_kw::<E>(b"AND")?;
            let hi = self.expr_bp::<E>(BP_REL)?;
            return Ok(Expr::Between(Box::new(l), Box::new(lo), Box::new(hi), neg));
        }
        Err(perr(self.cur().offset, ParseKind::UnexpectedToken))
    }

    /// The right side of `x [NOT] IN`: a table name (`v16`), a
    /// parenthesised subquery, or an expression list.
    fn expr_in<E>(&mut self, l: Expr, neg: bool) -> Result<Expr, E> {
        // `x IN tablename` (v0.16, `v16`): the pin reads it as
        // `x IN (SELECT * FROM tablename)` — a one-column table
        // is required at plan time.
        #[cfg(feature = "v16")]
        if let Tok::Ident(..) = self.cur().tok {
            let table = self.name::<E>()?;
            return Ok(Expr::InSubquery(
                Box::new(l),
                Box::new(self.table_select(table)),
                neg,
            ));
        }
        self.expect::<E>(Tok::LParen)?;
        // A subquery body: SELECT (or a v0.9 VALUES core),
        // possibly under further parentheses — the pin accepts
        // `IN ((SELECT ...))`.
        let save = self.pos;
        let mut extra_parens = 0usize;
        while matches!(self.cur().tok, Tok::LParen) {
            self.pos += 1;
            extra_parens += 1;
        }
        let is_subquery =
            self.cur_is_kw(b"SELECT") || (cfg!(feature = "v09") && self.cur_is_kw(b"VALUES"));
        if !is_subquery {
            // An expression list whose first item is
            // parenthesised: rewind and let the list parser see
            // the parens.
            self.pos = save;
            extra_parens = 0;
        }
        #[cfg(not(feature = "v06"))]
        let _ = extra_parens;
        if is_subquery {
            #[cfg(feature = "v06")]
            {
                let stmt = self.select_rich::<E>()?;
                for _ in 0..=extra_parens {
                    self.expect::<E>(Tok::RParen)?;
                }
                return Ok(Expr::InSubquery(Box::new(l), Box::new(stmt), neg));
            }
            #[cfg(not(feature = "v06"))]
            {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
        }
        let mut list = AVec::new();
        if !matches!(self.cur().tok, Tok::RParen) {
            loop {
                list.push(self.parse_expr::<E>()?);
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect::<E>(Tok::RParen)?;
        Ok(Expr::InList(Box::new(l), list, neg))
    }

    /// A prefix position: `NOT`, the unary operators, or a
    /// primary.
    fn expr_prefix<E>(&mut self) -> Result<Expr, E> {
        if self.eat_kw(b"NOT") {
            let e = self.expr_bp::<E>(BP_EQ)?;
            return Ok(Expr::Not(Box::new(e)));
        }
        self.expr_unary::<E>()
    }

    /// A LIKE ... ESCAPE / GLOB operator as the pin's function
    /// call (v0.16, `v16`): `name(pattern, subject[, escape])`,
    /// wrapped in NOT for the negated form.
    #[cfg(feature = "v16")]
    fn pattern_call<E>(
        &mut self,
        name: &[u8],
        pattern: Expr,
        subject: Expr,
        escape: Option<Expr>,
        neg: bool,
    ) -> Result<Expr, E> {
        let mut args = AVec::new();
        args.push(pattern);
        args.push(subject);
        if let Some(e) = escape {
            args.push(e);
        }
        let call = Expr::Func {
            name: self.arena.alloc_copy_handle(name)?,
            distinct: false,
            star: false,
            args,
        };
        Ok(if neg { Expr::Not(Box::new(call)) } else { call })
    }

    fn expr_unary<E>(&mut self) -> Result<Expr, E> {
        // Signed literals fold at this level (literal_at handles
        // the sign itself).
        if let Some((lit, used)) = self.literal_at(self.pos) {
            self.pos += used;
            return Ok(Expr::Literal(lit));
        }
        if self.eat(Tok::Minus) {
            let start = self.pos;
            let e = self.expr_prefix::<E>()?;
            // `-(9223372036854775808)`: parentheses (and unary
            // `+`) are invisible to the pin's integer rule — and
            // to its "hex literal too big" for `-` over
            // `0x8000000000000000` (v0.16).
            let toks = self.toks.as_slice().get(start..self.pos).unwrap_or(&[]);
            let only = |f: &dyn Fn(usize) -> bool| {
                toks.iter().enumerate().all(|(k, t)| {
                    matches!(t.tok, Tok::LParen | Tok::RParen | Tok::Plus) || f(start + k)
                })
            };
            if only(&|k| self.min_magnitude(k))
                && toks.iter().any(|t| matches!(t.tok, Tok::Real(_)))
            {
                return Ok(Expr::Literal(Literal::Integer(i64::MIN)));
            }
            if only(&|k| self.hex_min(k)) && toks.iter().any(|t| matches!(t.tok, Tok::Integer(_))) {
                return Err(perr(self.cur().offset, ParseKind::HexLiteralTooBig));
            }
            return Ok(Expr::Neg(Box::new(e)));
        }
        if self.eat(Tok::Plus) {
            let e = self.expr_prefix::<E>()?;
            return Ok(Expr::Pos(Box::new(e)));
        }
        #[cfg(feature = "v16")]
        if self.eat(Tok::BitNot) {
            let e = self.expr_prefix::<E>()?;
            return Ok(Expr::BitNot(Box::new(e)));
        }
        self.expr_primary::<E>()
    }

    fn expr_primary<E>(&mut self) -> Result<Expr, E> {
        #[cfg(feature = "v06")]
        if self.cur_is_kw(b"EXISTS") {
            self.pos += 1;
            self.expect::<E>(Tok::LParen)?;
            let mut extra_parens = 0usize;
            while matches!(self.cur().tok, Tok::LParen) {
                self.pos += 1;
                extra_parens += 1;
            }
            if !self.cur_is_kw(b"SELECT") && !(cfg!(feature = "v09") && self.cur_is_kw(b"VALUES")) {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
            let stmt = self.select_rich::<E>()?;
            for _ in 0..=extra_parens {
                self.expect::<E>(Tok::RParen)?;
            }
            return Ok(Expr::ExistsSubquery(Box::new(stmt)));
        }
        if self.eat(Tok::LParen) {
            if self.cur_is_kw(b"SELECT") || (cfg!(feature = "v09") && self.cur_is_kw(b"VALUES")) {
                #[cfg(feature = "v06")]
                {
                    let stmt = self.select_rich::<E>()?;
                    self.expect::<E>(Tok::RParen)?;
                    return Ok(Expr::ScalarSubquery(Box::new(stmt)));
                }
                #[cfg(not(feature = "v06"))]
                {
                    return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
                }
            }
            let e = self.parse_expr::<E>()?;
            // A row value `(a, b, ...)` (v0.16, `v16`).
            #[cfg(feature = "v16")]
            if self.eat(Tok::Comma) {
                let mut items = AVec::new();
                items.push(e);
                loop {
                    items.push(self.parse_expr::<E>()?);
                    if !self.eat(Tok::Comma) {
                        break;
                    }
                }
                self.expect::<E>(Tok::RParen)?;
                return Ok(Expr::Vector(items));
            }
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
                    && matches!(
                        self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
                        Some(Tok::LParen)
                    )
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
                    if !star && !matches!(self.cur().tok, Tok::RParen) {
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
                if matches!(self.cur().tok, Tok::Dot) {
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
            #[cfg(feature = "params")]
            Tok::Param(i, h) => {
                self.pos += 1;
                Ok(Expr::Param(i, h))
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    // ---------- rich SELECT ----------

    /// A full select statement (v0.9 shape): one core, an
    /// optional compound tail, and the trailing ORDER BY /
    /// LIMIT that belong to the whole statement. Without the
    /// `v09` feature this is exactly the pre-v0.9 single core
    /// plus its tail.
    fn select_rich<E>(&mut self) -> Result<SelectStmt, E> {
        let mut head = self.select_core::<E>()?;
        #[cfg(feature = "v09")]
        {
            let mut compound: AVec<(CompoundOp, SelectStmt)> = AVec::new();
            loop {
                let op = if self.eat_kw(b"UNION") {
                    if self.eat_kw(b"ALL") {
                        CompoundOp::UnionAll
                    } else {
                        CompoundOp::Union
                    }
                } else if self.eat_kw(b"EXCEPT") {
                    CompoundOp::Except
                } else if self.eat_kw(b"INTERSECT") {
                    CompoundOp::Intersect
                } else {
                    break;
                };
                compound.push((op, self.select_core::<E>()?));
            }
            // The pin's grammar: a trailing ORDER BY / LIMIT is
            // a syntax error when the LAST core is a VALUES core
            // (`VALUES (1) LIMIT 1`, `... UNION VALUES (2)
            // ORDER BY 1` both fail there) — reproduce it.
            let last_is_values = match compound.as_slice().last() {
                Some((_, c)) => !c.values.is_empty(),
                None => !head.values.is_empty(),
            };
            if last_is_values
                && (self.cur_is_kw(b"ORDER")
                    || self.cur_is_kw(b"LIMIT")
                    || self.cur_is_kw(b"OFFSET"))
            {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
            head.compound = compound;
        }
        self.select_tail::<E>(&mut head)?;
        Ok(head)
    }

    /// One compound operand: a SELECT core or — under `v09` —
    /// a VALUES core, with no ORDER BY / LIMIT / compound of
    /// its own (the pin gives cores none: `SELECT ... ORDER BY
    /// ... UNION ...` fails there with "ORDER BY clause should
    /// come after UNION").
    fn select_core<E>(&mut self) -> Result<SelectStmt, E> {
        #[cfg(feature = "v09")]
        if self.cur_is_kw(b"VALUES") {
            return self.values_core::<E>();
        }
        self.select_core_select::<E>()
    }

    /// A `VALUES (row), (row), ...` core (v0.9): values are
    /// full expressions, as in the INSERT grammar.
    #[cfg(feature = "v09")]
    fn values_core<E>(&mut self) -> Result<SelectStmt, E> {
        self.expect_kw::<E>(b"VALUES")?;
        let mut values = AVec::new();
        loop {
            self.expect::<E>(Tok::LParen)?;
            let mut row = AVec::new();
            loop {
                row.push(self.parse_expr::<E>()?);
                if self.eat(Tok::Comma) {
                    continue;
                }
                self.expect::<E>(Tok::RParen)?;
                break;
            }
            values.push(row);
            if self.eat(Tok::Comma) {
                continue;
            }
            break;
        }
        Ok(SelectStmt {
            distinct: false,
            items: AVec::new(),
            from: AVec::new(),
            filter: None,
            group_by: AVec::new(),
            having: None,
            order_by: AVec::new(),
            limit: None,
            offset: None,
            compound: AVec::new(),
            values,
        })
    }

    fn select_core_select<E>(&mut self) -> Result<SelectStmt, E> {
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
                && matches!(
                    self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
                    Some(Tok::Dot)
                )
                && matches!(
                    self.toks.as_slice().get(self.pos + 2).map(|t| t.tok),
                    Some(Tok::Star)
                )
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
        let from = if self.eat_kw(b"FROM") {
            self.source_list::<E>()?
        } else {
            AVec::new()
        };
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
        Ok(SelectStmt {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            order_by: AVec::new(),
            limit: None,
            offset: None,
            compound: AVec::new(),
            values: AVec::new(),
        })
    }

    /// The trailing ORDER BY / LIMIT of a select statement,
    /// parsed into `head` after its core(s) (v0.9 split: cores
    /// carry none of their own).
    fn select_tail<E>(&mut self, head: &mut SelectStmt) -> Result<(), E> {
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
                // NULLS FIRST / LAST (v0.16: honoured, where it
                // was parsed and ignored before — a silent wrong
                // answer for `ASC NULLS LAST` / `DESC NULLS
                // FIRST`). The pin's grammar needs FIRST or LAST
                // after NULLS; the default is NULL-smallest.
                let nulls_first = if self.eat_kw(b"NULLS") {
                    if self.eat_kw(b"FIRST") {
                        true
                    } else {
                        self.expect_kw::<E>(b"LAST")?;
                        false
                    }
                } else {
                    !desc
                };
                order_by.push((
                    e,
                    SortOrder {
                        desc,
                        nulls_first,
                        coll: Coll::Binary,
                    },
                ));
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        let mut limit = None;
        let mut offset = None;
        if self.eat_kw(b"LIMIT") {
            let first = self.limit_term::<E>()?;
            if self.eat(Tok::Comma) {
                // LIMIT offset, count
                offset = Some(first);
                limit = Some(self.limit_term::<E>()?);
            } else {
                limit = Some(first);
                if self.eat_kw(b"OFFSET") {
                    offset = Some(self.limit_term::<E>()?);
                }
            }
        }
        head.order_by = order_by;
        head.limit = limit;
        head.offset = offset;
        Ok(())
    }

    /// One LIMIT / OFFSET term: an integer literal (v0.4), or —
    /// under `v16` — any expression; the planner resolves it
    /// against an empty scope (the pin: a LIMIT may not name a
    /// column) and the executor converts its value with the
    /// pin's `MustBeInt` rule.
    fn limit_term<E>(&mut self) -> Result<Box<Expr>, E> {
        #[cfg(feature = "v16")]
        {
            Ok(Box::new(self.parse_expr::<E>()?))
        }
        #[cfg(not(feature = "v16"))]
        match self.literal_at(self.pos) {
            Some((lit @ Literal::Integer(_), used)) => {
                self.pos += used;
                Ok(Box::new(Expr::Literal(lit)))
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnsupportedClause)),
        }
    }

    /// `SELECT * FROM <table>` as an AST (v0.16: the subquery an
    /// `x IN table` operand stands for).
    #[cfg(feature = "v16")]
    fn table_select(&self, table: ABytes) -> SelectStmt {
        let mut items = AVec::new();
        items.push(SelectItem::Star);
        let mut from = AVec::new();
        from.push(FromSource {
            table,
            alias: None,
            kind: JoinKind::Inner,
            on: None,
            derived: None,
            natural: false,
            using: AVec::new(),
        });
        SelectStmt {
            distinct: false,
            items,
            from,
            filter: None,
            group_by: AVec::new(),
            having: None,
            order_by: AVec::new(),
            limit: None,
            offset: None,
            compound: AVec::new(),
            values: AVec::new(),
        }
    }

    /// A FROM clause body (v0.4; shared by SELECT and UPDATE
    /// ... FROM): the first source, then `,` / join-operator
    /// separated sources, each joined to everything on its left.
    fn source_list<E>(&mut self) -> Result<AVec<FromSource>, E> {
        let mut from = AVec::new();
        from.push(self.join_source::<E>(JoinKind::Inner, false, true)?);
        loop {
            if self.eat(Tok::Comma) {
                from.push(self.join_source::<E>(JoinKind::Cross, false, false)?);
                continue;
            }
            let Some((kind, natural)) = self.join_op::<E>()? else {
                break;
            };
            from.push(self.join_source::<E>(kind, natural, false)?);
        }
        Ok(from)
    }

    /// A join operator (v0.16, SQLite's `sqlite3JoinType`): up
    /// to three of NATURAL / LEFT / OUTER / RIGHT / FULL / INNER
    /// / CROSS, then JOIN. The flags combine as on the pin —
    /// LEFT + RIGHT is a FULL join, OUTER only qualifies LEFT /
    /// RIGHT / FULL (`OUTER JOIN`, `NATURAL OUTER JOIN` and
    /// `INNER OUTER JOIN` are "unknown join type" there), and
    /// INNER / CROSS with any outer flag is refused. Returns
    /// None when no join operator starts here.
    fn join_op<E>(&mut self) -> Result<Option<(JoinKind, bool)>, E> {
        const NATURAL: u8 = 1;
        const LEFT: u8 = 2;
        const OUTER: u8 = 4;
        const RIGHT: u8 = 8;
        const INNER: u8 = 16;
        const CROSS: u8 = 32;
        const WORDS: [(&[u8], u8); 7] = [
            (b"NATURAL", NATURAL),
            (b"LEFT", LEFT | OUTER),
            (b"OUTER", OUTER),
            (b"RIGHT", RIGHT | OUTER),
            (b"FULL", LEFT | RIGHT | OUTER),
            (b"INNER", INNER),
            (b"CROSS", INNER | CROSS),
        ];
        let mut flags = 0u8;
        let mut n = 0usize;
        while !self.cur_is_kw(b"JOIN") {
            let Some(code) = WORDS
                .iter()
                .find(|(w, _)| self.cur_is_kw(w))
                .map(|(_, c)| *c)
            else {
                if n == 0 {
                    return Ok(None);
                }
                return Err(perr(self.cur().offset, ParseKind::UnknownJoinType));
            };
            if n == 3 {
                return Err(perr(self.cur().offset, ParseKind::UnknownJoinType));
            }
            flags |= code;
            n += 1;
            self.pos += 1;
        }
        let at = self.cur().offset;
        self.pos += 1;
        if flags & (INNER | OUTER) == INNER | OUTER || flags & (OUTER | LEFT | RIGHT) == OUTER {
            return Err(perr(at, ParseKind::UnknownJoinType));
        }
        let kind = if flags & (LEFT | RIGHT) == LEFT | RIGHT {
            JoinKind::Full
        } else if flags & RIGHT != 0 {
            JoinKind::Right
        } else if flags & LEFT != 0 {
            JoinKind::Left
        } else if flags & CROSS != 0 {
            JoinKind::Cross
        } else {
            JoinKind::Inner
        };
        Ok(Some((kind, flags & NATURAL != 0)))
    }

    /// A joined source's constraint (v0.16): `ON <expr>`, or
    /// `USING (col, ...)`, or nothing. A NATURAL join may carry
    /// neither (the pin refuses it at prepare time); the first
    /// source carries none (`FROM t ON ...` is an error there
    /// too — the caller leaves the tokens unconsumed).
    fn join_constraint<E>(
        &mut self,
        natural: bool,
        first: bool,
    ) -> Result<(Option<Expr>, AVec<ABytes>), E> {
        let mut using = AVec::new();
        if first {
            return Ok((None, using));
        }
        let on = if self.eat_kw(b"ON") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        if on.is_none() && self.eat_kw(b"USING") {
            self.expect::<E>(Tok::LParen)?;
            loop {
                match self.cur().tok {
                    Tok::Ident(h, _) => {
                        self.pos += 1;
                        using.push(h);
                    }
                    _ => return Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
                }
                if self.eat(Tok::Comma) {
                    continue;
                }
                self.expect::<E>(Tok::RParen)?;
                break;
            }
        }
        if natural && (on.is_some() || !using.is_empty()) {
            return Err(perr(self.cur().offset, ParseKind::NaturalJoinConstraint));
        }
        Ok((on, using))
    }

    fn join_source<E>(
        &mut self,
        kind: JoinKind,
        natural: bool,
        first: bool,
    ) -> Result<FromSource, E> {
        // A derived table (v0.9): `(SELECT ...)` or `(VALUES
        // ...)`, possibly under further parentheses (the pin
        // accepts `FROM ((SELECT 1)) s`). The alias is optional;
        // no column-name list may follow it (pin: syntax error).
        #[cfg(feature = "v09")]
        if matches!(self.cur().tok, Tok::LParen) {
            let mut depth = 0usize;
            let mut i = self.pos;
            while i < self.limit && matches!(self.toks.as_slice()[i].tok, Tok::LParen) {
                depth += 1;
                i += 1;
            }
            if self.is_kw(i, b"SELECT") || self.is_kw(i, b"VALUES") {
                for _ in 0..depth {
                    self.expect::<E>(Tok::LParen)?;
                }
                let inner = self.select_rich::<E>()?;
                for _ in 0..depth {
                    self.expect::<E>(Tok::RParen)?;
                }
                let alias = self.opt_alias();
                let (on, using) = self.join_constraint::<E>(natural, first)?;
                return Ok(FromSource {
                    table: ABytes::EMPTY,
                    alias,
                    kind,
                    on,
                    derived: Some(Box::new(inner)),
                    natural,
                    using,
                });
            }
        }
        let table = self.name::<E>()?;
        let alias = self.opt_alias();
        let (on, using) = self.join_constraint::<E>(natural, first)?;
        Ok(FromSource {
            table,
            alias,
            kind,
            on,
            derived: None,
            natural,
            using,
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
        let mut checks: AVec<ABytes> = AVec::new();
        self.create_item::<E>(
            from,
            to,
            &mut columns,
            &mut pk_names,
            &mut pk_desc,
            &mut sets,
            &mut checks,
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
            checks,
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

// ===================== CREATE TABLE items =====================
//
// Every CREATE TABLE item (and the ALTER TABLE ADD COLUMN
// definition) is walked clause by clause with the pin's grammar
// (v0.16), so whatever is accepted — and later stored verbatim in
// `sqlite_schema` — is a definition the pin can load. A build
// without the v0.4 evaluator (the tiny profile) refuses the clauses
// it cannot enforce — CHECK, generated columns, expression
// DEFAULTs — with `ParseKind::RequiresEvaluator`.
impl<'a> Parser<'a> {
    /// The DEFAULT value starting at token `k`, with the index of
    /// the token after it (v0.16; the pin's grammar: `DEFAULT
    /// [+|-] term`, `DEFAULT (expr)`, `DEFAULT id`, where a term
    /// is a number, string, blob, NULL or a wall-clock keyword).
    /// `Ok(lit)` is a plain literal — a bare or parenthesised
    /// (signed) literal, TRUE / FALSE / NULL, or an identifier,
    /// which the pin stores as its text (`DEFAULT hello` is
    /// `'hello'`). `Err((start, end))` is the SQL byte span of an
    /// expression default: anything else in parentheses, a sign
    /// over a non-numeric term (`-'3'`, `-x'01'`), or
    /// CURRENT_TIME / CURRENT_DATE / CURRENT_TIMESTAMP (which a
    /// build without the evaluator refuses).
    #[allow(clippy::type_complexity)]
    fn default_clause<E>(
        &self,
        k: usize,
        to: usize,
    ) -> Result<(core::result::Result<Literal, (usize, usize)>, usize), E> {
        let toks = self.toks.as_slice();
        let Some(t) = toks.get(k).filter(|_| k < to).copied() else {
            return Err(perr(self.item_end_offset(k), ParseKind::UnexpectedToken));
        };
        let span = |a: usize, b: usize| Err((toks[a].offset, self.item_end_offset(b)));
        let term = |j: usize| {
            j < to
                && match toks[j].tok {
                    Tok::Integer(_) | Tok::Real(_) | Tok::Text(_) | Tok::Blob(_) => true,
                    Tok::Ident(h, false) => {
                        let b = handle_bytes(self.arena, h);
                        eq_ci(b, b"NULL") || is_ctime_kw(b)
                    }
                    _ => false,
                }
        };
        match t.tok {
            Tok::LParen => {
                // `(expr)`: the whole parenthesised group.
                let close = self.close_paren::<E>(k, to)?;
                if let Some((lit, n)) = self.literal_at(k + 1)
                    && k + 1 + n == close
                {
                    return Ok((Ok(lit), close + 1));
                }
                Ok((span(k + 1, close), close + 1))
            }
            // `[+|-] term`: exactly one sign (`- - 1` and `-TRUE`
            // are syntax errors on the pin).
            Tok::Minus | Tok::Plus if term(k + 1) => match self.literal_at(k) {
                Some((lit, 2)) => Ok((Ok(lit), k + 2)),
                _ => Ok((span(k, k + 2), k + 2)),
            },
            _ if term(k) => match self.literal_at(k) {
                Some((lit, _)) => Ok((Ok(lit), k + 1)),
                // A wall-clock keyword.
                None => Ok((span(k, k + 1), k + 1)),
            },
            // `id`: TRUE / FALSE, else the identifier's text.
            Tok::Ident(h, quoted)
                if quoted || !self.reserved(k, false) || self.is_kw(k, b"INDEXED") =>
            {
                if let Some((lit, _)) = self.literal_at(k) {
                    return Ok((Ok(lit), k + 1));
                }
                Ok((Ok(Literal::Text(h)), k + 1))
            }
            _ => Err(perr(t.offset, ParseKind::UnexpectedToken)),
        }
    }

    /// Whether token `i` is one of the pin's keywords that never
    /// fall back to an identifier (v0.16): such a word cannot be
    /// a column or constraint name, a type name word or a DEFAULT
    /// identifier — `CREATE TABLE t(a INT SELECT)` is a syntax
    /// error there, and storing it would leave a schema the pin
    /// cannot load. `nm` admits the join keywords and INDEXED
    /// (the pin's `nm ::= id | STRING | JOIN_KW`, `nm = true`); a
    /// type word (`ids`) takes neither, a DEFAULT `id` INDEXED
    /// only (its caller's special case).
    fn reserved(&self, i: usize, nm: bool) -> bool {
        let Some((h, false, _)) = self.ident_at(i) else {
            return false;
        };
        let w = handle_bytes(self.arena, h);
        let hit = |list: &str| list.split(' ').any(|k| eq_ci(k.as_bytes(), w));
        {
            if hit("CROSS FULL INNER LEFT NATURAL OUTER RIGHT") {
                return !nm;
            }
            if hit("INDEXED") {
                return !nm;
            }
            hit(
                "ADD ALL ALTER AND AS AUTOINCREMENT BETWEEN CASE CHECK COLLATE COMMIT \
CONSTRAINT CREATE DEFAULT DEFERRABLE DELETE DISTINCT DROP ELSE ESCAPE EXCEPT EXISTS FOREIGN \
FROM GROUP HAVING IN INDEX INSERT INTERSECT INTO IS ISNULL JOIN LIMIT NOT NOTHING NOTNULL NULL \
ON OR ORDER PRIMARY REFERENCES RETURNING SELECT SET TABLE THEN TO TRANSACTION UNION UNIQUE \
UPDATE USING VALUES WHEN WHERE",
            )
        }
    }

    /// Token `i` is a name (`nm`): an identifier that is not a
    /// reserved keyword, or a string.
    fn nm_at(&self, i: usize, to: usize) -> bool {
        i < to
            && match self.toks.as_slice()[i].tok {
                Tok::Ident(..) => !self.reserved(i, true),
                Tok::Text(_) => true,
                _ => false,
            }
    }

    /// Skip an optional `ON CONFLICT <action>` at token `i`,
    /// returning the next index. Parsed, not acted on (an
    /// honest gap: such a write errors where the pin resolves).
    fn onconf<E>(&self, i: usize, to: usize) -> Result<usize, E> {
        if !self.is_kw(i, b"ON") {
            return Ok(i);
        }
        if i + 2 < to
            && self.is_kw(i + 1, b"CONFLICT")
            && [
                b"ROLLBACK" as &[u8],
                b"ABORT",
                b"FAIL",
                b"IGNORE",
                b"REPLACE",
            ]
            .iter()
            .any(|w| self.is_kw(i + 2, w))
        {
            return Ok(i + 3);
        }
        Err(perr(self.item_end_offset(i), ParseKind::UnexpectedToken))
    }

    /// Skip a `REFERENCES nm [(cols)] {MATCH nm | ON
    /// DELETE|UPDATE|INSERT action}` clause whose REFERENCES is
    /// token `i`, returning the next index (foreign keys are
    /// parsed, not enforced).
    fn references<E>(&self, mut i: usize, to: usize) -> Result<usize, E> {
        let bad = |j: usize| Err(perr(self.item_end_offset(j), ParseKind::UnexpectedToken));
        if !self.nm_at(i + 1, to) {
            return bad(i + 1);
        }
        i += 2;
        if i < to && matches!(self.toks.as_slice()[i].tok, Tok::LParen) {
            i = self.constraint_columns::<E>(i, to, false)?.1;
        }
        loop {
            if self.is_kw(i, b"MATCH") {
                if !self.nm_at(i + 1, to) {
                    return bad(i + 1);
                }
                i += 2;
            } else if self.is_kw(i, b"ON")
                && (self.is_kw(i + 1, b"DELETE")
                    || self.is_kw(i + 1, b"UPDATE")
                    || self.is_kw(i + 1, b"INSERT"))
            {
                i += 2;
                i += if self.is_kw(i, b"SET")
                    && (self.is_kw(i + 1, b"NULL") || self.is_kw(i + 1, b"DEFAULT"))
                    || self.is_kw(i, b"NO") && self.is_kw(i + 1, b"ACTION")
                {
                    2
                } else if self.is_kw(i, b"CASCADE") || self.is_kw(i, b"RESTRICT") {
                    1
                } else {
                    return bad(i);
                };
            } else {
                return Ok(i);
            }
        }
    }

    /// Skip an optional `[NOT] DEFERRABLE [INITIALLY DEFERRED |
    /// IMMEDIATE]` at token `i` (parsed, not enforced).
    fn deferrable(&self, mut i: usize) -> usize {
        let start = i;
        if self.is_kw(i, b"NOT") {
            i += 1;
        }
        if !self.is_kw(i, b"DEFERRABLE") {
            return start;
        }
        i += 1;
        if self.is_kw(i, b"INITIALLY")
            && (self.is_kw(i + 1, b"DEFERRED") || self.is_kw(i + 1, b"IMMEDIATE"))
        {
            i += 2;
        }
        i
    }

    /// Parse a `(col [COLLATE name] [ASC|DESC], ...)` list whose
    /// `(` is token `i` (table-level PRIMARY KEY / UNIQUE, a
    /// FOREIGN KEY's or REFERENCES' column list), returning the
    /// columns and the index after the `)`. `autoinc` admits the
    /// PRIMARY KEY's `AUTOINCREMENT` before the `)`. Anything
    /// else — an expression, a missing `)` — is a syntax error
    /// (v0.16: the list used to stop silently at the first
    /// unexpected token and keep the columns before it).
    fn constraint_columns<E>(
        &self,
        mut i: usize,
        to: usize,
        autoinc: bool,
    ) -> Result<(AVec<IndexColumnDef>, usize), E> {
        let mut out = AVec::new();
        let tok = |j: usize| {
            self.toks
                .as_slice()
                .get(j)
                .filter(|_| j < to)
                .map(|t| t.tok)
        };
        if !matches!(tok(i), Some(Tok::LParen)) {
            return Err(perr(self.item_end_offset(i), ParseKind::UnexpectedToken));
        }
        i += 1;
        loop {
            let h = match self.ident_at(i) {
                Some((h, _, _)) if i < to && !self.reserved(i, true) => h,
                _ => return Err(perr(self.item_end_offset(i), ParseKind::UnexpectedToken)),
            };
            i += 1;
            // `col [COLLATE name] [ASC | DESC]` — the pin's
            // order (v0.16: the collation is the autoindex key's;
            // it used to be skipped, and only after ASC / DESC,
            // so `(a COLLATE x DESC, b)` lost `b`).
            #[cfg(any(feature = "v04", feature = "indexes"))]
            let mut coll = None;
            if self.is_kw(i, b"COLLATE") {
                #[cfg(any(feature = "v04", feature = "indexes"))]
                {
                    coll = Some(self.coll_at::<E>(i + 1)?);
                }
                i += 2;
            }
            let mut desc = false;
            if self.is_kw(i, b"DESC") {
                desc = true;
                i += 1;
            } else if self.is_kw(i, b"ASC") {
                i += 1;
            }
            out.push(IndexColumnDef {
                name: h,
                desc,
                #[cfg(any(feature = "v04", feature = "indexes"))]
                coll,
            });
            match tok(i) {
                Some(Tok::Comma) => i += 1,
                _ => break,
            }
        }
        if autoinc && self.is_kw(i, b"AUTOINCREMENT") {
            i += 1;
        }
        match tok(i) {
            Some(Tok::RParen) => Ok((out, i + 1)),
            _ => Err(perr(self.item_end_offset(i), ParseKind::UnexpectedToken)),
        }
    }

    /// One comma-separated item of a CREATE TABLE body (or the
    /// column of an ALTER TABLE ADD COLUMN), walked token by token
    /// with the pin's grammar (v0.16: the clauses used to be found
    /// by scanning for keywords, so trailing junk — `CHECK(a>0)
    /// bar`, `DEFAULT abs(1)`, `PRIMARY KEY (a) foo` — was
    /// accepted and stored verbatim in `sqlite_schema`, a schema
    /// the pin then refuses to load). Returns whether the item is
    /// a table constraint; after one, a column definition is a
    /// syntax error (the caller's check). Table constraints may
    /// follow each other without a comma, as on the pin.
    #[allow(clippy::too_many_arguments)]
    fn create_item<E>(
        &mut self,
        from: usize,
        to: usize,
        columns: &mut AVec<ColumnDef>,
        pk_names: &mut AVec<ABytes>,
        pk_desc_names: &mut AVec<ABytes>,
        sets: &mut AVec<ConstraintSet>,
        #[cfg(feature = "v04")] checks: &mut AVec<ABytes>,
    ) -> Result<bool, E> {
        let bad = |j: usize| -> Result<bool, E> {
            Err(perr(self.item_end_offset(j), ParseKind::UnexpectedToken))
        };
        let tok = |j: usize| {
            self.toks
                .as_slice()
                .get(j)
                .filter(|_| j < to)
                .map(|t| t.tok)
        };
        if from >= to {
            return bad(from);
        }
        let tcons = ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"]
            .iter()
            .any(|w| self.is_kw(from, w.as_bytes()));
        if tcons {
            let mut i = from;
            while i < to {
                if self.is_kw(i, b"CONSTRAINT") {
                    if !self.nm_at(i + 1, to) {
                        return bad(i + 1);
                    }
                    i += 2;
                } else if self.is_kw(i, b"PRIMARY") || self.is_kw(i, b"UNIQUE") {
                    // Table-level PRIMARY KEY / UNIQUE (col, …)
                    // (v0.3: UNIQUE enforced via an implicit
                    // index, like SQLite).
                    let pk = self.is_kw(i, b"PRIMARY");
                    if pk {
                        if !self.is_kw(i + 1, b"KEY") {
                            return bad(i + 1);
                        }
                        i += 1;
                    }
                    let (cols, next) = self.constraint_columns::<E>(i + 1, to, pk)?;
                    if pk {
                        for c in cols.iter() {
                            pk_names.push(c.name);
                            if c.desc {
                                pk_desc_names.push(c.name);
                            }
                        }
                    }
                    sets.push(ConstraintSet {
                        primary_key: pk,
                        columns: cols,
                    });
                    i = self.onconf::<E>(next, to)?;
                } else if self.is_kw(i, b"CHECK") {
                    // Table-level CHECK (v0.16): enforced on every
                    // write path through the v0.4 evaluator; a
                    // build without it refuses the table rather
                    // than ignore the check.
                    #[cfg(feature = "v04")]
                    {
                        let (close, text) = self.paren_text::<E>(i + 1, to)?;
                        checks.push(text);
                        i = self.onconf::<E>(close + 1, to)?;
                    }
                    #[cfg(not(feature = "v04"))]
                    return Err(perr(
                        self.toks.as_slice()[i].offset,
                        ParseKind::RequiresEvaluator,
                    ));
                } else if self.is_kw(i, b"FOREIGN") && self.is_kw(i + 1, b"KEY") {
                    // Parsed, not enforced (see module docs).
                    let next = self.constraint_columns::<E>(i + 2, to, false)?.1;
                    if !self.is_kw(next, b"REFERENCES") {
                        return bad(next);
                    }
                    i = self.references::<E>(next, to)?;
                    i = self.deferrable(i);
                } else {
                    return bad(i);
                }
            }
            return Ok(true);
        }
        // Column definition: first token is the name.
        let col_name = match self.ident_at(from) {
            Some((h, _, _)) if self.nm_at(from, to) => h,
            _ => return bad(from),
        };
        // Declared type (`typetoken`): name words, then an
        // optional `(n)` / `(n, m)` of signed numbers; the text is
        // the SQL span. `GENERATED` and `ALWAYS` are type words
        // (`b GENERATED AS (a)` declares type GENERATED, `a
        // GENERATED ALWAYS (1)` type `GENERATED ALWAYS (1)`) except
        // in `GENERATED ALWAYS AS`, whose words the pin strips
        // from the type.
        let mut i = from + 1;
        while i < to
            && match self.toks.as_slice()[i].tok {
                Tok::Text(_) => true,
                Tok::Ident(..) => {
                    !self.reserved(i, false)
                        && !(self.is_kw(i, b"GENERATED")
                            && self.is_kw(i + 1, b"ALWAYS")
                            && self.is_kw(i + 2, b"AS"))
                }
                _ => false,
            }
        {
            i += 1;
        }
        if i > from + 1 && matches!(tok(i), Some(Tok::LParen)) {
            i += 1;
            for n in 0..2 {
                if matches!(tok(i), Some(Tok::Plus | Tok::Minus)) {
                    i += 1;
                }
                if !matches!(tok(i), Some(Tok::Integer(_) | Tok::Real(_))) {
                    return bad(i);
                }
                i += 1;
                if n == 0 && matches!(tok(i), Some(Tok::Comma)) {
                    i += 1;
                    continue;
                }
                break;
            }
            if !matches!(tok(i), Some(Tok::RParen)) {
                return bad(i);
            }
            i += 1;
        }
        let decl_end_tok = i;
        let mut pk_count = 0usize;
        let mut pk_desc = false;
        let mut unique = false;
        let mut default: Option<Literal> = None;
        let mut not_null = false;
        #[cfg(any(feature = "v04", feature = "indexes"))]
        let mut coll = None;
        // Without the evaluator (tiny) neither can become true:
        // the clauses that set them are refused.
        #[cfg(feature = "v04")]
        let mut default_nonconst = false;
        #[cfg(not(feature = "v04"))]
        let default_nonconst = false;
        #[cfg(feature = "v04")]
        let mut generated = false;
        #[cfg(not(feature = "v04"))]
        let generated = false;
        #[cfg(feature = "v04")]
        let mut default_expr: Option<ABytes> = None;
        #[cfg(feature = "v04")]
        let mut gen_expr: Option<ABytes> = None;
        #[cfg(feature = "v04")]
        let mut stored = false;
        // Column constraints (`carglist`), in any order.
        while i < to {
            if self.is_kw(i, b"CONSTRAINT") {
                if !self.nm_at(i + 1, to) {
                    return bad(i + 1);
                }
                i += 2;
            } else if self.is_kw(i, b"DEFAULT") {
                let (d, next) = self.default_clause::<E>(i + 1, to)?;
                match d {
                    Ok(lit) => {
                        default = Some(lit);
                        #[cfg(feature = "v04")]
                        {
                            default_nonconst = false;
                            default_expr = None;
                        }
                    }
                    // An expression DEFAULT (v0.16): kept as text
                    // for the planner's evaluator; a build without
                    // one refuses the table (it used to keep the
                    // first literal of `DEFAULT (1+2)` and NULL for
                    // the rest).
                    #[cfg(feature = "v04")]
                    Err((start, end)) => {
                        default = None;
                        default_nonconst = true;
                        let text = trim(&self.sql[start.min(end)..end.min(self.sql.len())]);
                        default_expr = Some(self.arena.alloc_copy_handle(text)?);
                    }
                    #[cfg(not(feature = "v04"))]
                    Err(_) => {
                        return Err(perr(
                            self.toks.as_slice()[i + 1].offset,
                            ParseKind::RequiresEvaluator,
                        ));
                    }
                }
                i = next;
            } else if self.is_kw(i, b"NULL") {
                i = self.onconf::<E>(i + 1, to)?;
            } else if self.is_kw(i, b"NOT") && self.is_kw(i + 1, b"NULL") {
                not_null = true;
                i = self.onconf::<E>(i + 2, to)?;
            } else if self.is_kw(i, b"NOT") || self.is_kw(i, b"DEFERRABLE") {
                let next = self.deferrable(i);
                if next == i {
                    return bad(i + 1);
                }
                i = next;
            } else if self.is_kw(i, b"PRIMARY") {
                if !self.is_kw(i + 1, b"KEY") {
                    return bad(i + 1);
                }
                pk_count += 1;
                i += 2;
                if self.is_kw(i, b"DESC") {
                    pk_desc = true;
                    i += 1;
                } else if self.is_kw(i, b"ASC") {
                    i += 1;
                }
                i = self.onconf::<E>(i, to)?;
                if self.is_kw(i, b"AUTOINCREMENT") {
                    i += 1;
                }
            } else if self.is_kw(i, b"UNIQUE") {
                unique = true;
                i = self.onconf::<E>(i + 1, to)?;
            } else if self.is_kw(i, b"CHECK") {
                // Column-level CHECK (v0.16): a table check like
                // any other.
                #[cfg(feature = "v04")]
                {
                    let (close, text) = self.paren_text::<E>(i + 1, to)?;
                    checks.push(text);
                    i = close + 1;
                }
                #[cfg(not(feature = "v04"))]
                return Err(perr(
                    self.toks.as_slice()[i].offset,
                    ParseKind::RequiresEvaluator,
                ));
            } else if self.is_kw(i, b"REFERENCES") {
                i = self.references::<E>(i, to)?;
            } else if self.is_kw(i, b"COLLATE") {
                // v0.16: the column's collating sequence (it used
                // to be parsed and ignored — comparisons and
                // ORDER BY ran BINARY).
                #[cfg(any(feature = "v04", feature = "indexes"))]
                {
                    coll = Some(self.coll_at::<E>(i + 1)?);
                }
                if !matches!(tok(i + 1), Some(Tok::Ident(..) | Tok::Text(_))) {
                    return bad(i + 1);
                }
                i += 2;
            } else if self.is_kw(i, b"AS")
                || (self.is_kw(i, b"GENERATED") && self.is_kw(i + 1, b"ALWAYS"))
            {
                let at = if self.is_kw(i, b"AS") {
                    i
                } else if self.is_kw(i + 2, b"AS") {
                    i + 2
                } else {
                    return bad(i + 2);
                };
                // Every generated column has `AS (...)`; without
                // the evaluator it cannot be computed.
                #[cfg(not(feature = "v04"))]
                return Err(perr(
                    self.toks.as_slice()[at].offset,
                    ParseKind::RequiresEvaluator,
                ));
                // A generated column (v0.16): `[GENERATED ALWAYS]
                // AS (expr) [VIRTUAL | STORED]`. It used to be
                // parsed as part of the declared type and read as
                // NULL.
                #[cfg(feature = "v04")]
                {
                    if generated {
                        return bad(i);
                    }
                    generated = true;
                    let (close, text) = self.paren_text::<E>(at + 1, to)?;
                    gen_expr = Some(text);
                    stored = self.is_kw(close + 1, b"STORED");
                    i = close + 1 + usize::from(stored || self.is_kw(close + 1, b"VIRTUAL"));
                }
            } else {
                return bad(i);
            }
        }
        if pk_count > 1 {
            // SQLite: "table has more than one primary key".
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        let pk = pk_count == 1;
        let decl = if decl_end_tok > from + 1 {
            let start_off = self.toks.as_slice()[from + 1].offset;
            let end_off = self.item_end_offset(decl_end_tok);
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
            #[cfg(feature = "v04")]
            default_expr,
            #[cfg(feature = "v04")]
            gen_expr,
            #[cfg(feature = "v04")]
            stored,
            #[cfg(any(feature = "v04", feature = "indexes"))]
            coll,
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
                #[cfg(any(feature = "v04", feature = "indexes"))]
                coll: None,
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
                #[cfg(any(feature = "v04", feature = "indexes"))]
                coll: None,
            });
            sets.push(ConstraintSet {
                primary_key: false,
                columns: cols,
            });
        }
        Ok(false)
    }
}

/// CURRENT_TIME / CURRENT_DATE / CURRENT_TIMESTAMP (v0.16): the
/// pin's wall-clock keywords, an expression wherever a DEFAULT
/// term may be.
fn is_ctime_kw(b: &[u8]) -> bool {
    eq_ci(b, b"CURRENT_TIME") || eq_ci(b, b"CURRENT_DATE") || eq_ci(b, b"CURRENT_TIMESTAMP")
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
                if matches!(self.cur().tok, Tok::Dot) {
                    return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
                }
                Ok(h)
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    /// An UPDATE / upsert `SET` list: `col = expr, ...`. Under
    /// `v16` an item may be `(c1, ..., cn) = <row>` (v0.16),
    /// split here into one assignment per column: a row value of
    /// size n gives each column its element, any other
    /// right-hand side gives column i `RowItem(rhs, i, n)` (the
    /// planner requires a scalar subquery of n columns). A size
    /// mismatch is the pin's "N columns assigned M values".
    fn set_list<E>(&mut self) -> Result<AVec<(ABytes, Expr)>, E> {
        let mut assignments: AVec<(ABytes, Expr)> = AVec::new();
        loop {
            #[cfg(feature = "v16")]
            if self.eat(Tok::LParen) {
                let mut cols: AVec<ABytes> = AVec::new();
                loop {
                    cols.push(self.plain_name::<E>()?);
                    if !self.eat(Tok::Comma) {
                        break;
                    }
                }
                self.expect::<E>(Tok::RParen)?;
                self.expect::<E>(Tok::Eq)?;
                let e = self.parse_expr::<E>()?;
                let n = u16::try_from(cols.len()).map_err(|_| Error::Schema {
                    kind: crate::error::SchemaKind::ColumnCountMismatch,
                })?;
                match e {
                    Expr::Vector(items) => {
                        if items.len() != cols.len() {
                            return Err(Error::Schema {
                                kind: crate::error::SchemaKind::ColumnCountMismatch,
                            });
                        }
                        for (c, x) in cols.iter().zip(items.iter()) {
                            assignments.push((*c, x.clone()));
                        }
                    }
                    e if n == 1 => {
                        if let Some(c) = cols.as_slice().first() {
                            assignments.push((*c, e));
                        }
                    }
                    e => {
                        let rhs = Box::new(e);
                        for (i, c) in cols.iter().enumerate() {
                            let i = u16::try_from(i).unwrap_or(u16::MAX);
                            assignments.push((*c, Expr::RowItem(rhs.clone(), i, n)));
                        }
                    }
                }
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
            let col = self.plain_name::<E>()?;
            self.expect::<E>(Tok::Eq)?;
            let e = self.parse_expr::<E>()?;
            assignments.push((col, e));
            if self.eat(Tok::Comma) {
                continue;
            }
            break;
        }
        Ok(assignments)
    }

    fn update_stmt<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"UPDATE")?;
        // OR clause: under v05 alone only IGNORE widens behaviour
        // (ABORT/FAIL/ROLLBACK are the default); OR REPLACE is a
        // v0.7 addition, rejected by name without the feature.
        let mut conflict = ConflictAction::Abort;
        if self.eat_kw(b"OR") {
            if self.eat_kw(b"IGNORE") {
                conflict = ConflictAction::Ignore;
            } else if self.cur_is_kw(b"REPLACE") {
                #[cfg(feature = "v07")]
                {
                    self.pos += 1;
                    conflict = ConflictAction::Replace;
                }
                #[cfg(not(feature = "v07"))]
                {
                    return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
                }
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
        let assignments = self.set_list::<E>()?;
        // FROM (v0.7): sources in SELECT shape, after SET and
        // before WHERE, exactly as SQLite orders the clauses.
        #[cfg(feature = "v07")]
        let from = self.update_from::<E>()?;
        #[cfg(not(feature = "v07"))]
        let from: AVec<FromSource> = AVec::new();
        let filter = if self.eat_kw(b"WHERE") {
            Some(self.parse_expr::<E>()?)
        } else {
            None
        };
        // ORDER BY/LIMIT on UPDATE stay rejected (the pinned
        // oracle's build rejects them too); expect_end in parse()
        // catches any trailing input.
        #[cfg(feature = "v07")]
        let returning = self.returning_clause::<E>()?;
        #[cfg(not(feature = "v07"))]
        let returning: AVec<SelectItem> = AVec::new();
        Ok(Ast::Update {
            table,
            alias,
            conflict,
            assignments,
            from,
            filter,
            returning,
        })
    }

    /// The optional `FROM <source> [, <source> | JOIN ...]` of
    /// an UPDATE (v0.7): the same source grammar as a SELECT's
    /// FROM clause body.
    #[cfg(feature = "v07")]
    fn update_from<E>(&mut self) -> Result<AVec<FromSource>, E> {
        if !self.eat_kw(b"FROM") {
            return Ok(AVec::new());
        }
        self.source_list::<E>()
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
        #[cfg(feature = "v07")]
        let returning = self.returning_clause::<E>()?;
        #[cfg(not(feature = "v07"))]
        let returning: AVec<SelectItem> = AVec::new();
        Ok(Ast::Delete {
            table,
            filter,
            returning,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Scope;

    fn kind(sql: &str) -> Option<ParseKind> {
        let arena = Arena::new(Scope::Statement, 0, 1 << 16);
        match parse::<()>(sql.as_bytes(), &arena) {
            Ok(_) => None,
            Err(Error::Parse { kind, .. }) => Some(kind),
            Err(_) => Some(ParseKind::UnexpectedToken),
        }
    }

    /// v0.16: the tiny profile (no evaluator) refuses every table
    /// clause it cannot enforce or compute, instead of parsing and
    /// ignoring it (they used to be silent wrong answers: CHECK
    /// never enforced, `DEFAULT (1+2)` stored 1, generated columns
    /// read NULL) — and still accepts the literal forms it handles.
    #[cfg(not(feature = "v04"))]
    #[test]
    fn tiny_refuses_unenforceable_table_clauses() {
        let refused = [
            "CREATE TABLE t(a CHECK(a > 0))",
            "CREATE TABLE t(a, CHECK(a > 0))",
            "CREATE TABLE t(a, CONSTRAINT c CHECK(a > 0))",
            "CREATE TABLE t(a, b AS (a * 2))",
            "CREATE TABLE t(a, b GENERATED ALWAYS AS (a) STORED)",
            "CREATE TABLE t(a DEFAULT (1 + 2))",
            "CREATE TABLE t(a DEFAULT -'3')",
            "CREATE TABLE t(a DEFAULT CURRENT_TIMESTAMP)",
            "CREATE TABLE t(a DEFAULT current_date)",
            "CREATE TABLE t(a DEFAULT -current_time)",
        ];
        for sql in refused {
            assert_eq!(kind(sql), Some(ParseKind::RequiresEvaluator), "{sql}");
        }
        for sql in [
            "CREATE TABLE t(a DEFAULT 5, b DEFAULT -1.5, c DEFAULT 'x', d DEFAULT x'00')",
            "CREATE TABLE t(a DEFAULT (5), b DEFAULT (-2), c DEFAULT TRUE, d DEFAULT NULL)",
            "CREATE TABLE t(a DEFAULT +'x', b DEFAULT \"quoted\", c INTEGER NOT NULL)",
            "CREATE TABLE t(a DEFAULT hello, b DEFAULT key)",
        ] {
            assert_eq!(kind(sql), None, "{sql}");
        }
    }

    /// v0.16, every build: CREATE TABLE items follow the pin's
    /// grammar clause by clause. Trailing junk after a constraint
    /// used to be accepted and stored verbatim, a schema the pin
    /// then refuses to load ("malformed database schema").
    #[test]
    fn create_table_items_follow_the_pins_grammar() {
        for sql in [
            "CREATE TABLE t(a INT PRIMARY KEY foo)",
            "CREATE TABLE t(a DEFAULT \"x\" y)",
            "CREATE TABLE t(a DEFAULT - - 1)",
            "CREATE TABLE t(a DEFAULT 'a' 'b')",
            "CREATE TABLE t(a DEFAULT x.y)",
            "CREATE TABLE t(a DEFAULT \"x\"(1))",
            "CREATE TABLE t(a DEFAULT (1)(2))",
            "CREATE TABLE t(a DEFAULT TRUE FALSE)",
            "CREATE TABLE t(a DEFAULT)",
            "CREATE TABLE t(a DEFAULT SELECT)",
            "CREATE TABLE t(a, PRIMARY KEY (a) foo)",
            "CREATE TABLE t(a, PRIMARY KEY a)",
            "CREATE TABLE t(a, b, PRIMARY KEY (a, b + 1))",
            "CREATE TABLE t(a, UNIQUE (a) (b))",
            "CREATE TABLE t(a, UNIQUE (a AUTOINCREMENT))",
            "CREATE TABLE t(a, FOREIGN KEY (a))",
            "CREATE TABLE t(a, PRIMARY KEY (a), b)",
            "CREATE TABLE t(a, CONSTRAINT c, b)",
            "CREATE TABLE t(a, INDEX (a))",
            "CREATE TABLE t(a VARCHAR(10) foo)",
            "CREATE TABLE t(a VARCHAR(10, 20, 30))",
            "CREATE TABLE t(a VARCHAR(x))",
            "CREATE TABLE t(a (10))",
            "CREATE TABLE t(a INT SELECT)",
            "CREATE TABLE t(SELECT INT)",
            "CREATE TABLE t(a PRIMARY)",
            "CREATE TABLE t(a PRIMARY KEY KEY)",
            "CREATE TABLE t(a NOT)",
            "CREATE TABLE t(a ON CONFLICT IGNORE)",
            "CREATE TABLE t(a UNIQUE ON CONFLICT)",
            "CREATE TABLE t(a REFERENCES)",
            "CREATE TABLE t(a REFERENCES p(x) foo)",
            "CREATE TABLE t(a REFERENCES p MATCH)",
            "CREATE TABLE t(a REFERENCES p ON DELETE NO)",
            "CREATE TABLE t(a COLLATE)",
            "CREATE TABLE t(a, )",
            "CREATE TABLE t(, a)",
            "CREATE TABLE t(a PRIMARY KEY PRIMARY KEY)",
            "CREATE TABLE t(a, b, PRIMARY KEY (a) PRIMARY KEY (b))",
        ] {
            assert!(kind(sql).is_some(), "{sql}");
        }
        for sql in [
            "CREATE TABLE t(a 'text' \"int\" [x] `y`, b VARCHAR(-10, +20.5))",
            "CREATE TABLE t(a DOUBLE PRECISION(10), b UNSIGNED BIG INT, c KEY)",
            "CREATE TABLE t(a, key INT, natural, indexed, left)",
            "CREATE TABLE t(a INT PRIMARY KEY ASC ON CONFLICT IGNORE, b UNIQUE ON CONFLICT ABORT)",
            "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL ON CONFLICT REPLACE)",
            "CREATE TABLE t(a NULL NOT NULL DEFAULT 1 DEFAULT 2 CONSTRAINT c CONSTRAINT 'd')",
            "CREATE TABLE t(a REFERENCES p (x) ON DELETE CASCADE ON UPDATE SET NULL \
             MATCH FULL ON INSERT NO ACTION NOT DEFERRABLE INITIALLY IMMEDIATE, b DEFERRABLE)",
            "CREATE TABLE t(a, b, PRIMARY KEY (a DESC, b ASC) ON CONFLICT FAIL UNIQUE (b) \
             CONSTRAINT f FOREIGN KEY (a, b) REFERENCES p ON DELETE SET DEFAULT \
             DEFERRABLE INITIALLY DEFERRED, CONSTRAINT lone)",
            "CREATE TABLE t(a INTEGER, PRIMARY KEY (a AUTOINCREMENT))",
            "CREATE TABLE t(a DEFAULT 0x10, b DEFAULT -0x10, c DEFAULT +NULL, d DEFAULT indexed)",
            "CREATE TABLE t(a GENERATED, b GENERATED ALWAYS (1))",
        ] {
            assert_eq!(kind(sql), None, "{sql}");
        }
    }

    /// v0.16: `-9223372036854775808` is the INTEGER i64::MIN (the
    /// pin's `codeInteger`), through unary `+`; `- -` over it, a
    /// REAL spelling or a larger magnitude stay REAL.
    #[test]
    fn minimum_integer_literal() {
        let arena = Arena::new(Scope::Statement, 0, 1 << 16);
        let lit = |sql: &str| {
            let toks = lexer::lex::<()>(sql.as_bytes(), &arena).ok()?;
            let p = Parser {
                limit: toks.len(),
                toks,
                pos: 0,
                arena: &arena,
                sql: sql.as_bytes(),
            };
            p.literal_at(0).map(|(l, _)| l)
        };
        for sql in [
            "-9223372036854775808",
            "- 09223372036854775808",
            "-+9223372036854775808",
        ] {
            assert!(
                matches!(lit(sql), Some(Literal::Integer(i64::MIN))),
                "{sql}"
            );
        }
        for sql in [
            "- -9223372036854775808",
            "-9223372036854775809",
            "-9223372036854775808.0",
            "-9223372036854775808e0",
            "9223372036854775808",
        ] {
            assert!(matches!(lit(sql), Some(Literal::Real(_))), "{sql}");
        }
    }

    /// v0.16: the stored schema text restarts at the bare created
    /// name, after the pin's own keyword prefix (the pin cannot load
    /// `CREATE TABLE main.t`).
    #[test]
    fn schema_text_starts_at_the_bare_name() {
        let arena = Arena::new(Scope::Statement, 0, 1 << 16);
        for (sql, prefix, at) in [
            (
                &b"create table if not exists main . t(a)"[..],
                &b"CREATE TABLE "[..],
                34,
            ),
            (b"CREATE TABLE t(a)", b"CREATE TABLE ", 13),
            (
                b"create unique index \"main\".i on t(a)",
                b"CREATE UNIQUE INDEX ",
                27,
            ),
            (b"create  index i on t(a)", b"CREATE INDEX ", 14),
            (b"create view v as select 1", b"CREATE VIEW ", 12),
        ] {
            assert_eq!(
                schema_text_start::<()>(sql, &arena).ok(),
                Some(Some((prefix, at)))
            );
        }
        let auto = b"CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT)";
        assert!(matches!(
            schema_text_start::<()>(auto, &arena),
            Err(Error::Unsupported)
        ));
    }

    /// v0.16 (evaluator builds): the clauses are kept for the
    /// planner, and the pin's syntax errors around them hold.
    #[cfg(feature = "v04")]
    #[test]
    fn table_clauses_parse_with_the_pins_grammar() {
        for sql in [
            "CREATE TABLE t(a CHECK(a > 0), b, CHECK(a < b))",
            "CREATE TABLE t(a, b AS (a * 2) STORED, c GENERATED ALWAYS AS (b) VIRTUAL NOT NULL)",
            "CREATE TABLE t(a DEFAULT (1 + 2), b DEFAULT -'3', c DEFAULT CURRENT_TIME, d DEFAULT hello)",
        ] {
            assert_eq!(kind(sql), None, "{sql}");
        }
        for sql in [
            "CREATE TABLE t(a DEFAULT 1 + 2)",
            "CREATE TABLE t(a DEFAULT -TRUE)",
            "CREATE TABLE t(a, b AS (a) VIRTUAL STORED)",
            "CREATE TABLE t(a, b GENERATED ALWAYS (a))",
            "CREATE TABLE t(a CHECK)",
        ] {
            assert!(kind(sql).is_some(), "{sql}");
        }
    }

    /// v0.16: collation clauses — the column's, a table UNIQUE's
    /// (before ASC / DESC; the clause used to be skipped only
    /// after them, so `(a COLLATE x DESC, b)` dropped `b`), a
    /// CREATE INDEX key's — and unknown names refused.
    #[cfg(feature = "indexes")]
    #[test]
    fn collation_clauses_parse_and_unknown_names_are_refused() {
        let arena = Arena::new(Scope::Statement, 0, 1 << 16);
        let sql = "CREATE TABLE t(a TEXT COLLATE NOCASE, b COLLATE 'rtrim', \
                   UNIQUE (a COLLATE BINARY DESC, b))";
        let Ok(Ast::CreateTable {
            columns,
            constraint_sets,
            ..
        }) = parse::<()>(sql.as_bytes(), &arena)
        else {
            panic!("parses");
        };
        assert_eq!(columns.as_slice()[0].coll, Some(Coll::NoCase));
        assert_eq!(columns.as_slice()[1].coll, Some(Coll::Rtrim));
        let set = &constraint_sets.as_slice()[0];
        assert_eq!(set.columns.len(), 2);
        assert_eq!(set.columns.as_slice()[0].coll, Some(Coll::Binary));
        assert!(set.columns.as_slice()[0].desc);
        let Ok(Ast::CreateIndex { columns, .. }) =
            parse::<()>(b"CREATE INDEX i ON t(a COLLATE \"NoCase\" DESC)", &arena)
        else {
            panic!("parses");
        };
        assert_eq!(columns.as_slice()[0].coll, Some(Coll::NoCase));
        for sql in [
            "CREATE TABLE t(a COLLATE foo)",
            "CREATE TABLE t(a, UNIQUE (a COLLATE foo))",
            "CREATE INDEX i ON t(a COLLATE foo)",
        ] {
            assert!(
                matches!(
                    parse::<()>(sql.as_bytes(), &arena),
                    Err(Error::Schema {
                        kind: crate::error::SchemaKind::UnknownCollation
                    })
                ),
                "{sql}"
            );
        }
    }

    /// v0.16 (`v16`): the COLLATE operator binds tighter than
    /// `||` and looser than unary minus; GLOB and LIKE ... ESCAPE
    /// are the pin's `glob()` / `like()` calls.
    #[cfg(feature = "v16")]
    #[test]
    fn collate_operator_and_pattern_calls() {
        let arena = Arena::new(Scope::Statement, 0, 1 << 16);
        let e = parse_expr_text::<()>(b"'a' || -b COLLATE nocase", &arena).expect("parses");
        let Expr::Binary(BinOp::Concat, _, r) = e else {
            panic!("concat on top");
        };
        assert!(matches!(*r, Expr::Collate(ref x, Coll::NoCase) if matches!(**x, Expr::Neg(_))));
        let e = parse_expr_text::<()>(b"x NOT GLOB 'a*'", &arena).expect("parses");
        let Expr::Not(call) = e else {
            panic!("NOT GLOB negates the call");
        };
        assert!(matches!(*call, Expr::Func { ref args, .. } if args.len() == 2));
        let e = parse_expr_text::<()>(b"x LIKE 'a%' ESCAPE '\\'", &arena).expect("parses");
        assert!(matches!(e, Expr::Func { ref args, .. } if args.len() == 3));
        assert!(parse_expr_text::<()>(b"x GLOB 'a' ESCAPE 'b'", &arena).is_err());
    }
}
