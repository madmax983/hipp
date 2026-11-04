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

/// How deep parentheses may nest inside one CREATE TABLE item
/// (v0.16: the pin's parser stack; it was 32, refusing CHECK and
/// DEFAULT expressions the pin takes — the expression itself is
/// then held to `Limits`).
const MAX_DEPTH: usize = PIN_PARSER_DEPTH;

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
    /// The nesting limits (v0.16; a zero-sized pin constant
    /// without the evaluator, whose grammar does not nest).
    lim: Limits,
    /// The pin's LALR parser stack while the current construct is
    /// parsed: how many grammar symbols sit below it (v0.16; see
    /// `Parser::need`).
    #[cfg(feature = "v04")]
    held: usize,
    /// The pin's tree height (`Expr.nHeight`) of the expression
    /// just parsed, and what kind of node it is (`EK_*`).
    #[cfg(feature = "v04")]
    eh: usize,
    #[cfg(feature = "v04")]
    ek: u8,
    /// The largest resolution depth (`Parser::sel_r`) of the
    /// subqueries met so far in the current resolved expression.
    #[cfg(feature = "v04")]
    rsub: usize,
    /// The select just parsed: its height as a subquery operand
    /// (`heightOfSelect`) and its resolution depth — the deepest
    /// sum of nested expression heights the pin's name resolution
    /// reaches inside it (`Parse.nHeight`).
    #[cfg(feature = "v04")]
    sel_h: usize,
    #[cfg(feature = "v04")]
    sel_r: usize,
    /// The select being parsed: its height and resolution depth so
    /// far (`sel_h` / `sel_r` once it is complete).
    #[cfg(feature = "v04")]
    acc_h: usize,
    #[cfg(feature = "v04")]
    acc_r: usize,
    /// The current core's ON / USING terms, which the pin ANDs
    /// into its WHERE before resolving it (`sqlite3ProcessJoin`).
    #[cfg(feature = "v04")]
    on: OnChain,
    /// Resolution depths are computed but not checked: a CREATE
    /// VIEW's body, which the pin resolves only when the view is
    /// read (`parse_view` reads one that way).
    #[cfg(feature = "v04")]
    defer: bool,
    /// Parsing a view's text to read it (`parse_view`).
    #[cfg(feature = "v04")]
    view_use: bool,
    /// Parsing a CREATE VIEW body (v0.16 review fix): the pin keeps
    /// a view's tree in its reduced form, whose leaves lose their
    /// height, so a leaf the pin resolves on its own (`SELECT a`,
    /// `(SELECT 1)`'s `1`) adds 0 to the resolution depth there, 1
    /// elsewhere (probed: `SELECT <998 signs>(SELECT 1)` reads as a
    /// view, as a CTE only up to 997).
    #[cfg(feature = "v04")]
    reduced: bool,
    /// The tallest result column of the select core just parsed.
    #[cfg(feature = "v04")]
    core_h: usize,
    /// Parsing a trigger program (v0.16, `triggers`): `RAISE()` is
    /// legal only here.
    #[cfg(feature = "triggers")]
    in_trig: bool,
}

/// The ON / USING terms of one select core (v0.16): the pin's
/// `sqlite3ProcessJoin` makes the WHERE clause `((WHERE AND on1)
/// AND on2) ...` before name resolution, so the tree it resolves
/// is taller than any one term. Kept as a fold, not a list: the
/// terms' count, the first one's height, the best `height + 1 -
/// position` of the others, and their deepest subquery
/// resolution.
#[cfg(feature = "v04")]
#[derive(Debug, Clone, Copy, Default)]
struct OnChain {
    n: usize,
    first: usize,
    rest: isize,
    rsub: usize,
}

#[cfg(feature = "v04")]
impl OnChain {
    fn push(&mut self, h: usize, rsub: usize) {
        self.n += 1;
        if self.n == 1 {
            self.first = h;
            self.rest = isize::MIN;
        } else {
            let v = isize::try_from(h)
                .unwrap_or(isize::MAX)
                .saturating_add(1)
                .saturating_sub(isize::try_from(self.n).unwrap_or(isize::MAX));
            self.rest = self.rest.max(v);
        }
        self.rsub = self.rsub.max(rsub);
    }

    /// The height of the WHERE tree the pin resolves, given the
    /// written WHERE's height (if any).
    fn height(&self, w: Option<usize>) -> usize {
        let rest = |off: isize| usize::try_from(self.rest.saturating_add(off)).unwrap_or(0);
        match (w, self.n) {
            (Some(hw), 0) => hw,
            (None, 0) => 0,
            (Some(hw), n) => n.saturating_add(hw.max(self.first).max(rest(0))),
            (None, n) => (n - 1).saturating_add(self.first.max(rest(1))),
        }
    }
}

/// The parser's nesting limits (v0.16), the pin's
/// `SQLITE_LIMIT_EXPR_DEPTH` and its parser stack depth. Deeper
/// statements are refused (`ParseKind::ExprTooDeep`,
/// `ParseKind::RecursionLimit`), never a stack overflow: every
/// recursive walk of a statement is bounded by them. Only the v0.4
/// grammar nests (parentheses, operators, subqueries, ...); without
/// it the limits are the pin's constants and the type is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The deepest expression tree, counted as the pin counts
    /// `Expr.nHeight` (the pin: 1000; `Config::max_expr_depth`).
    #[cfg(feature = "v04")]
    pub expr_depth: usize,
    /// The deepest parser stack, counted in the pin's LALR
    /// grammar symbols (the pin: 2500; `Config::max_parser_depth`).
    #[cfg(feature = "v04")]
    pub parser_depth: usize,
}

impl Limits {
    /// The pin's limits: an expression tree 1000 deep, a parser
    /// stack of 2500 symbols.
    pub const PIN: Limits = Limits {
        #[cfg(feature = "v04")]
        expr_depth: PIN_EXPR_DEPTH,
        #[cfg(feature = "v04")]
        parser_depth: PIN_PARSER_DEPTH,
    };

    /// The expression-height limit (the pin's without the v0.4
    /// grammar).
    fn expr(&self) -> usize {
        #[cfg(feature = "v04")]
        {
            self.expr_depth
        }
        #[cfg(not(feature = "v04"))]
        {
            PIN_EXPR_DEPTH
        }
    }

    /// The parser-stack limit (likewise).
    fn stack(&self) -> usize {
        #[cfg(feature = "v04")]
        {
            self.parser_depth
        }
        #[cfg(not(feature = "v04"))]
        {
            PIN_PARSER_DEPTH
        }
    }
}

/// The pin's `SQLITE_MAX_EXPR_DEPTH` ("Expression tree is too large
/// (maximum depth 1000)").
pub const PIN_EXPR_DEPTH: usize = 1000;
/// The pin's parser stack ("Recursion limit"): 2500 LALR symbols,
/// probed (2493 nested parentheses in a result column parse, 2494
/// do not).
pub const PIN_PARSER_DEPTH: usize = 2500;
/// The pin's `SQLITE_MAX_COLUMN`: columns of a table, an index or
/// a result set, terms of an ORDER BY / GROUP BY, items of a SET.
pub const MAX_COLUMN: usize = 2000;
/// The pin's `SQLITE_MAX_FUNCTION_ARG` (this build: 1000).
#[cfg(feature = "v04")]
pub const MAX_FUNCTION_ARG: usize = 1000;
/// The pin's `SQLITE_MAX_COMPOUND_SELECT`.
#[cfg(feature = "v09")]
pub const MAX_COMPOUND_SELECT: usize = 500;
/// The pin's join limit ("at most 64 tables in a join", the
/// planner's bitmask width).
#[cfg(feature = "v04")]
pub const MAX_JOIN_TABLES: usize = 64;
/// The pin's `SQLITE_MAX_SQL_LENGTH` ("statement too long").
pub const MAX_SQL_LENGTH: usize = 1_000_000_000;

// Expression kinds (`Parser::ek`), as the pin's parse actions see
// them.
/// No column reference and no subquery: the pin's
/// `sqlite3ExprIsConstant` at parse time.
#[cfg(feature = "v04")]
const EK_CONST: u8 = 1;
/// A numeric, string or blob literal, possibly under unary `+` /
/// `-`: `x ISNULL` over it folds to a literal.
#[cfg(feature = "v04")]
const EK_LIT: u8 = 2;
/// A unary `+` node (the pin re-uses it for a `+` / `-` above).
#[cfg(feature = "v04")]
const EK_UPLUS: u8 = 4;
/// A COLLATE node (an IN list item the pin does not rewrite).
#[cfg(feature = "v04")]
const EK_COLLATE: u8 = 8;

/// Record `h`, the height of a whole expression the pin resolves on
/// its own (a result column, a WHERE tree, ...), on every subquery
/// directly inside `e` (`SelectStmt::rctx`; v0.16 depth limits,
/// `v16`): resolving that subquery's expressions starts that much
/// deeper. Subqueries' own expressions were marked when they were
/// parsed; the walk does not enter them, so each node is visited
/// once per statement. Iterative: `e` may be as deep as the parser
/// stack allows.
#[cfg(feature = "v16")]
fn mark_ctx(e: &mut Expr, h: usize) {
    let h32 = u32::try_from(h).unwrap_or(u32::MAX);
    let mut todo: AVec<&mut Expr> = AVec::new();
    todo.push(e);
    while let Some(x) = todo.pop() {
        match x {
            Expr::Literal(_) | Expr::Column { .. } | Expr::HexTooBig(_) => {}
            #[cfg(feature = "params")]
            Expr::Param(..) => {}
            #[cfg(any(feature = "triggers", feature = "foreign_keys"))]
            Expr::TrigValue(..) => {}
            #[cfg(feature = "triggers")]
            Expr::Raise(_, None, _) => {}
            #[cfg(feature = "triggers")]
            Expr::Raise(_, Some(a), _) => todo.push(a),
            Expr::Binary(_, a, b) | Expr::Like(a, b, _) => {
                todo.push(a);
                todo.push(b);
            }
            Expr::Neg(a)
            | Expr::Pos(a)
            | Expr::Not(a)
            | Expr::IsNull(a, _)
            | Expr::Cast(a, _)
            | Expr::Collate(a, _)
            | Expr::BitNot(a)
            | Expr::RowItem(a, ..) => todo.push(a),
            Expr::Vector(l) => {
                for y in l.as_mut_slice() {
                    todo.push(y);
                }
            }
            Expr::InList(a, l, _) => {
                todo.push(a);
                for y in l.as_mut_slice() {
                    todo.push(y);
                }
            }
            Expr::InSubquery(a, q, _) => {
                q.rctx = h32;
                todo.push(a);
            }
            Expr::ScalarSubquery(q) | Expr::ExistsSubquery(q) => q.rctx = h32,
            Expr::Between(a, b, c, _) => {
                todo.push(a);
                todo.push(b);
                todo.push(c);
            }
            Expr::Case {
                base,
                whens,
                otherwise,
            } => {
                if let Some(y) = base {
                    todo.push(y);
                }
                for (w, t) in whens.as_mut_slice() {
                    todo.push(w);
                    todo.push(t);
                }
                if let Some(y) = otherwise {
                    todo.push(y);
                }
            }
            Expr::Func { args, clause, .. } => {
                for y in args.as_mut_slice() {
                    todo.push(y);
                }
                if let Some(c) = clause {
                    #[cfg(feature = "window")]
                    if let Some(crate::sql::ast::Over::Spec(w)) = &mut c.over {
                        // Window terms are shallow (each was checked
                        // as it was parsed): a nested walk is bounded.
                        w.exprs_mut(&mut |y| mark_ctx(y, h));
                    }
                    for (y, _) in c.order_by.as_mut_slice() {
                        todo.push(y);
                    }
                    if let Some(y) = &mut c.filter {
                        todo.push(y);
                    }
                }
            }
        }
    }
}

/// Make an ORDER BY / GROUP BY term that the pin's
/// `sqlite3ExprIsInteger` reads as an integer the integer literal
/// the planner takes for an ordinal (see `Parser::sort_item`).
/// `word`: the term spells TRUE or FALSE, whose literal the pin
/// never reads as an integer; a bare one is wrapped in a unary `+`
/// so that it stays a constant.
#[cfg(feature = "v04")]
fn ordinal_term(e: &mut Expr, word: bool) {
    #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
    let mut t = e;
    #[cfg(feature = "v16")]
    while let Expr::Collate(inner, _) = t {
        t = inner;
    }
    let mut neg = false;
    let mut x: &Expr = t;
    loop {
        match x {
            Expr::Pos(a) => x = a,
            Expr::Neg(a) => {
                neg = !neg;
                x = a;
            }
            _ => break,
        }
    }
    let Expr::Literal(Literal::Integer(v)) = x else {
        return;
    };
    let v = *v;
    if word {
        if matches!(t, Expr::Literal(_)) {
            let lit = core::mem::replace(t, Expr::Literal(Literal::Null));
            *t = Expr::Pos(Box::new(lit));
        }
        return;
    }
    // The pin's integer literals are 32-bit there (`EP_IntValue`).
    if (-0x7fff_ffff..=0x7fff_ffff).contains(&v) {
        *t = Expr::Literal(Literal::Integer(if neg { -v } else { v }));
    }
}

/// What supplies an INSERT's rows (v0.8): VALUES rows, or a
/// SELECT source (exactly one of the two is meaningful —
/// `select` is `Some` iff the source was a SELECT, in which
/// case `rows` is empty).
struct InsertSource {
    rows: AVec<AVec<Expr>>,
    select: Option<Box<SelectStmt>>,
}

/// Parse one statement under the nesting limits `lim` (v0.16; see
/// `Limits`).
pub fn parse<E>(sql: &[u8], arena: &Arena, lim: Limits) -> Result<Ast, E> {
    let toks = lex_checked::<E>(sql, arena)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    let ast = p.statement::<E>()?;
    p.expect_end::<E>()?;
    Ok(ast)
}

/// `parse` of a stored CREATE VIEW text that is about to be read
/// (v0.16): the body's resolution depth is checked, as the pin
/// checks it when it resolves the view; `parse` of a CREATE VIEW
/// does not (the pin creates such a view and refuses only its
/// readers).
#[cfg(feature = "v04")]
pub fn parse_view<E>(sql: &[u8], arena: &Arena, lim: Limits) -> Result<Ast, E> {
    let toks = lex_checked::<E>(sql, arena)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    p.view_use = true;
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
pub fn parse_params<E>(
    sql: &[u8],
    arena: &Arena,
    lim: Limits,
) -> Result<(Ast, lexer::ParamTable), E> {
    let mut params = lexer::ParamTable::default();
    too_long::<E>(sql.len())?;
    let toks = lexer::lex_params::<E>(sql, arena, &mut params)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    let ast = p.statement::<E>()?;
    p.expect_end::<E>()?;
    let stored = match &ast {
        #[cfg(feature = "v16")]
        Ast::CreateTable { as_select, .. } => as_select.is_none(),
        #[cfg(not(feature = "v16"))]
        Ast::CreateTable { .. } => true,
        Ast::AlterAddColumn { .. } | Ast::CreateView { .. } => true,
        // "parameters prohibited in index expressions / partial
        // index WHERE clauses" (v0.16, `expr_index`).
        #[cfg(feature = "indexes")]
        Ast::CreateIndex { .. } => true,
        // "trigger cannot use variables" (v0.16, `triggers`).
        #[cfg(feature = "triggers")]
        Ast::CreateTrigger(_) => true,
        _ => false,
    };
    if stored && params.count > 0 {
        return Err(Error::Schema {
            kind: crate::error::SchemaKind::ParameterNotAllowed,
        });
    }
    Ok((ast, params))
}

/// `parse`, also returning the statement's tokens (v0.16, `v16`):
/// every name in the tree is the handle of the token it was read
/// from, so ALTER TABLE can map a resolved name back to its bytes
/// in `sql` (`Token::offset`).
#[cfg(feature = "v16")]
pub fn parse_with_tokens<E>(
    sql: &[u8],
    arena: &Arena,
    lim: Limits,
) -> Result<(Ast, AVec<Token>), E> {
    let toks = lex_checked::<E>(sql, arena)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    let ast = p.statement::<E>()?;
    p.expect_end::<E>()?;
    Ok((ast, p.toks))
}

/// `parse_expr_text`, also returning the expression's tokens
/// (v0.16, `v16`; see `parse_with_tokens`).
#[cfg(feature = "v16")]
pub fn parse_expr_with_tokens<E>(
    sql: &[u8],
    arena: &Arena,
    lim: Limits,
) -> Result<(Expr, AVec<Token>), E> {
    let toks = lex_checked::<E>(sql, arena)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    p.held = RULE_HELD;
    let e = p.parse_expr::<E>()?;
    p.resolved::<E>()?;
    if !p.at_eof() {
        return Err(perr(p.cur().offset, ParseKind::TrailingInput));
    }
    Ok((e, p.toks))
}

/// The tokens of `sql` (v0.16, `v16`): ALTER TABLE walks a stored
/// CREATE INDEX text by tokens (an expression or partial index is
/// outside hipp's CREATE INDEX grammar).
#[cfg(feature = "v16")]
pub fn tokens<E>(sql: &[u8], arena: &Arena) -> Result<AVec<Token>, E> {
    lexer::lex::<E>(sql, arena)
}

/// Parse one stand-alone expression (v0.16): the stored text of
/// a CHECK constraint, generated column or DEFAULT, re-parsed by
/// the planner into `arena`. The whole input must be the one
/// expression.
#[cfg(feature = "v04")]
pub fn parse_expr_text<E>(sql: &[u8], arena: &Arena, lim: Limits) -> Result<Expr, E> {
    let toks = lex_checked::<E>(sql, arena)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    p.held = RULE_HELD;
    let e = p.parse_expr::<E>()?;
    p.resolved::<E>()?;
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
/// Without `v16` it also refuses AUTOINCREMENT
/// (`Error::Unsupported`): those builds do not maintain the
/// companion `sqlite_sequence` table, and a file holding an
/// AUTOINCREMENT table without it is "malformed" to the pin on its
/// next INSERT. The INSERT planner calls it on the target table's
/// stored text for the same reason (an existing AUTOINCREMENT
/// table is read-only there). With `v16` AUTOINCREMENT is
/// maintained (`has_autoincrement`). The word is reserved, so an
/// unquoted occurrence is the clause.
pub fn schema_text_start<E>(
    sql: &[u8],
    arena: &Arena,
) -> Result<Option<(&'static [u8], usize)>, E> {
    let Ok(toks) = lexer::lex::<E>(sql, arena) else {
        return Ok(None);
    };
    let p = Parser::new(toks, arena, sql, Limits::PIN);
    #[cfg(not(feature = "v16"))]
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

/// Whether a stored CREATE TABLE text declares AUTOINCREMENT
/// (v0.16, `v16`): the word is reserved, so an unquoted
/// occurrence can only be the PRIMARY KEY's clause. Text that
/// does not lex has none.
#[cfg(feature = "v16")]
pub fn has_autoincrement<E>(sql: &[u8], arena: &Arena) -> bool {
    let Ok(toks) = lexer::lex::<E>(sql, arena) else {
        return false;
    };
    let p = Parser::new(toks, arena, sql, Limits::PIN);
    (0..p.limit).any(|i| p.is_kw(i, b"AUTOINCREMENT"))
}

/// Which constraint kinds of a `CREATE TABLE` text carry a
/// constraint-level `ON CONFLICT ROLLBACK` (v0.16, `v16`): (a NOT
/// NULL constraint, a UNIQUE or PRIMARY KEY constraint). hipp
/// otherwise parses constraint-level conflict clauses without
/// acting on them; this one decides whether a violation ends the
/// whole transaction. Unlexable text: neither.
#[cfg(feature = "v16")]
pub fn rollback_constraints<E>(sql: &[u8], arena: &Arena) -> (bool, bool) {
    let Ok(toks) = lexer::lex::<E>(sql, arena) else {
        return (false, false);
    };
    let p = Parser::new(toks, arena, sql, Limits::PIN);
    let tok = |i: usize| p.toks.as_slice().get(i).map(|t| t.tok);
    let (mut nn, mut uq) = (false, false);
    for i in 1..p.limit {
        if !(p.is_kw(i, b"ON") && p.is_kw(i + 1, b"CONFLICT") && p.is_kw(i + 2, b"ROLLBACK")) {
            continue;
        }
        // Back from the clause to the constraint's keyword: past a
        // table constraint's column list and a PRIMARY KEY's order.
        let mut j = i - 1;
        if tok(j) == Some(Tok::RParen) {
            let mut depth = 0usize;
            loop {
                match tok(j) {
                    Some(Tok::RParen) => depth += 1,
                    Some(Tok::LParen) => depth = depth.saturating_sub(1),
                    _ => {}
                }
                if depth == 0 || j == 0 {
                    break;
                }
                j -= 1;
            }
            j = j.saturating_sub(1);
        }
        if p.is_kw(j, b"ASC") || p.is_kw(j, b"DESC") {
            j = j.saturating_sub(1);
        }
        if p.is_kw(j, b"NULL") && j > 0 && p.is_kw(j - 1, b"NOT") {
            nn = true;
        } else if p.is_kw(j, b"UNIQUE") || p.is_kw(j, b"KEY") {
            uq = true;
        }
    }
    (nn, uq)
}

/// The pin's `SQLITE_LIMIT_SQL_LENGTH` check (v0.16): a statement
/// of more than 1,000,000,000 bytes is "statement too long". Only
/// with the v0.4 grammar: the tiny profile's targets cannot hold
/// such a statement (and its size ratchet pays for none).
fn too_long<E>(len: usize) -> Result<(), E> {
    #[cfg(feature = "v04")]
    if len > MAX_SQL_LENGTH {
        return Err(perr(0, ParseKind::StatementTooLong));
    }
    #[cfg(not(feature = "v04"))]
    let _ = len;
    Ok(())
}

/// `lexer::lex` after the length check.
fn lex_checked<E>(sql: &[u8], arena: &Arena) -> Result<AVec<Token>, E> {
    too_long::<E>(sql.len())?;
    lexer::lex::<E>(sql, arena)
}

/// The pin's parser stack below a CHECK / DEFAULT / generated
/// expression of a table's first column (or first table
/// constraint): the bottom entry, `create_table`, `(`,
/// `columnname`, `carglist`, `CHECK`, `(` — probed: 2491 nested
/// parentheses parse there. A later column's sits two symbols
/// higher on the pin (`columnlist ,`); hipp re-parses the stored
/// text alone and counts the first column's.
#[cfg(feature = "v04")]
const RULE_HELD: usize = 7;

/// The pin's parser stack below an INSERT's row source: the
/// bottom entry, `with`, `insert_cmd`, `INTO`, `xfullname`,
/// `idlist_opt`.
const INS_BASE: usize = 6;
/// The pin's stack below a CREATE INDEX key item (`createkw
/// uniqueflag INDEX ifnotexists nm dbnm ON nm LP sortlist`), v0.16
/// (`expr_index`).
#[cfg(feature = "expr_index")]
const IDX_HELD: usize = 10;
/// ... and below a RETURNING item of an INSERT without an upsert
/// clause (`select RETURNING sclp scanpt`).
#[cfg(feature = "v07")]
const INS_RETURNING: usize = 10;

impl<'a> Parser<'a> {
    fn new(toks: AVec<Token>, arena: &'a Arena, sql: &'a [u8], lim: Limits) -> Parser<'a> {
        Parser {
            limit: toks.len(),
            toks,
            pos: 0,
            arena,
            sql,
            lim,
            // The pin's bottom stack entry (state 0).
            #[cfg(feature = "v04")]
            held: 1,
            #[cfg(feature = "v04")]
            eh: 0,
            #[cfg(feature = "v04")]
            ek: 0,
            #[cfg(feature = "v04")]
            rsub: 0,
            #[cfg(feature = "v04")]
            sel_h: 0,
            #[cfg(feature = "v04")]
            sel_r: 0,
            #[cfg(feature = "v04")]
            acc_h: 0,
            #[cfg(feature = "v04")]
            acc_r: 0,
            #[cfg(feature = "v04")]
            on: OnChain {
                n: 0,
                first: 0,
                rest: 0,
                rsub: 0,
            },
            #[cfg(feature = "v04")]
            defer: false,
            #[cfg(feature = "v04")]
            view_use: false,
            #[cfg(feature = "v04")]
            reduced: false,
            #[cfg(feature = "v04")]
            core_h: 0,
            #[cfg(feature = "triggers")]
            in_trig: false,
        }
    }

    /// `sql[a..b]`, or nothing when the range is not inside it.
    fn span(&self, a: usize, b: usize) -> &'a [u8] {
        self.sql.get(a..b.min(self.sql.len())).unwrap_or(&[])
    }

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
                // v0.16 (`v16`): `temp.` too — the connection routes a
                // statement naming the temp schema to its temp
                // database (`session::temp`) before it plans it, so
                // the planner never reads `temp.t` as `main.t`.
                if matches!(self.cur().tok, Tok::Dot) {
                    let s = handle_bytes(self.arena, h);
                    if !eq_ci(s, b"main") && !(cfg!(feature = "v16") && eq_ci(s, b"temp")) {
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

    /// Whether token `i` starts a `WITH` clause (v0.16, `v16`;
    /// always false without it, so the clause stays a syntax
    /// error there).
    fn with_at(&self, i: usize) -> bool {
        cfg!(feature = "v16") && self.is_kw(i, b"WITH")
    }

    /// `WITH [RECURSIVE] cte [, cte ...]` (v0.16, `v16`; the pin's
    /// grammar): each CTE is `name [(col, ...)] AS [[NOT]
    /// MATERIALIZED] (select-stmt)`.
    #[cfg(feature = "v16")]
    ///
    /// `hs` symbols sit below the WITH on the pin's stack; a body's
    /// resolution depth is checked where the body is read (see
    /// `SelectStmt::rdepth`).
    #[inline(never)]
    fn with_clause<E>(&mut self, hs: usize) -> Result<crate::sql::ast::With, E> {
        self.expect_kw::<E>(b"WITH")?;
        let rec = usize::from(self.eat_kw(b"RECURSIVE"));
        let mut ctes = AVec::new();
        loop {
            let name = match self.cur().tok {
                Tok::Ident(h, _) => {
                    self.pos += 1;
                    h
                }
                _ => return Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
            };
            let mut columns = AVec::new();
            if self.eat(Tok::LParen) {
                loop {
                    match self.cur().tok {
                        Tok::Ident(h, _) => {
                            self.pos += 1;
                            columns.push(h);
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
            self.expect_kw::<E>(b"AS")?;
            let hint = if self.eat_kw(b"NOT") {
                self.expect_kw::<E>(b"MATERIALIZED")?;
                crate::sql::ast::M10d::No
            } else if self.eat_kw(b"MATERIALIZED") {
                crate::sql::ast::M10d::Yes
            } else {
                crate::sql::ast::M10d::Any
            };
            self.expect::<E>(Tok::LParen)?;
            // `WITH [RECURSIVE] [wqlist COMMA] withnm eidlist_opt
            // wqas LP select`.
            // The pin resolves a CTE body only where it is read
            // (`defer`; an unused one never), inside the reader's
            // expressions: the expansion checks its resolution
            // depth (`rdepth`) there, and its width.
            let (h0, d0) = (self.held, self.defer);
            self.held = hs + rec + if ctes.is_empty() { 5 } else { 7 };
            self.defer = true;
            let r = self
                .peak::<E>(self.held)
                .and_then(|()| self.select_rich::<E>());
            (self.held, self.defer) = (h0, d0);
            let select = r?;
            self.expect::<E>(Tok::RParen)?;
            ctes.push(crate::sql::ast::Cte {
                name,
                columns,
                select,
                hint,
            });
            if !self.eat(Tok::Comma) {
                break;
            }
        }
        Ok(crate::sql::ast::With { ctes })
    }

    /// A statement led by a WITH clause (v0.16, `v16`): a select
    /// statement keeps the clause itself; INSERT / REPLACE /
    /// UPDATE / DELETE are wrapped in `Ast::With`.
    #[cfg(feature = "v16")]
    fn with_statement<E>(&mut self) -> Result<Ast, E> {
        let save = self.pos;
        let with = self.with_clause::<E>(self.held)?;
        if self.cur_is_kw(b"SELECT") || self.cur_is_kw(b"VALUES") {
            self.pos = save;
            return Ok(Ast::Select(self.select_rich::<E>()?));
        }
        let body = if self.cur_is_kw(b"INSERT") {
            self.insert::<E>(false)?
        } else if self.cur_is_kw(b"REPLACE") {
            self.replace_dispatch::<E>()?
        } else if self.cur_is_kw(b"UPDATE") {
            self.update_dispatch::<E>()?
        } else if self.cur_is_kw(b"DELETE") {
            self.delete_dispatch::<E>()?
        } else {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        };
        Ok(Ast::With {
            with: Box::new(with),
            body: Box::new(body),
        })
    }

    /// `name`, also saying whether the `main.` qualifier was
    /// written (v0.16, `v16`: a qualified name never denotes a
    /// CTE).
    #[cfg(feature = "v16")]
    fn name_q<E>(&mut self) -> Result<(ABytes, bool), E> {
        let qualified = matches!(
            self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
            Some(Tok::Dot)
        );
        Ok((self.name::<E>()?, qualified))
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
            } else if cfg!(feature = "triggers") && self.is_kw(self.pos + 1, b"TRIGGER") {
                #[cfg(feature = "triggers")]
                {
                    self.create_trigger::<E>()
                }
                #[cfg(not(feature = "triggers"))]
                {
                    Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
                }
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
        } else if self.with_at(self.pos) {
            #[cfg(feature = "v16")]
            {
                self.with_statement::<E>()
            }
            #[cfg(not(feature = "v16"))]
            {
                Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
            }
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
            #[cfg(feature = "v16")]
            let immediate = !self.eat_kw(b"DEFERRED")
                && (self.eat_kw(b"IMMEDIATE") || self.eat_kw(b"EXCLUSIVE"));
            #[cfg(not(feature = "v16"))]
            let _ =
                self.eat_kw(b"DEFERRED") || self.eat_kw(b"IMMEDIATE") || self.eat_kw(b"EXCLUSIVE");
            let _ = self.eat_kw(b"TRANSACTION");
            #[cfg(feature = "v16")]
            if immediate {
                return Ok(Ast::BeginImmediate);
            }
            Ok(Ast::Begin)
        } else if self.cur_is_kw(b"COMMIT") || self.cur_is_kw(b"END") {
            self.pos += 1;
            let _ = self.eat_kw(b"TRANSACTION");
            Ok(Ast::Commit)
        } else if self.cur_is_kw(b"ROLLBACK") {
            self.pos += 1;
            let _ = self.eat_kw(b"TRANSACTION");
            // ROLLBACK TO [SAVEPOINT] <name> (v0.16, `v16`).
            if self.cur_is_kw(b"TO") {
                #[cfg(feature = "v16")]
                {
                    self.pos += 1;
                    let _ = self.eat_kw(b"SAVEPOINT");
                    let name = self.pragma_nm::<E>()?;
                    return Ok(Ast::RollbackTo { name });
                }
                #[cfg(not(feature = "v16"))]
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
            Ok(Ast::Rollback)
        } else if cfg!(feature = "v16")
            && (self.cur_is_kw(b"SAVEPOINT")
                || self.cur_is_kw(b"RELEASE")
                || self.cur_is_kw(b"VACUUM")
                || self.cur_is_kw(b"REINDEX")
                || self.cur_is_kw(b"ANALYZE"))
        {
            #[cfg(feature = "v16")]
            {
                self.maint_stmt::<E>()
            }
            #[cfg(not(feature = "v16"))]
            {
                Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
            }
        } else if cfg!(feature = "v16") && self.cur_is_kw(b"PRAGMA") {
            #[cfg(feature = "v16")]
            {
                self.pragma_stmt::<E>()
            }
            #[cfg(not(feature = "v16"))]
            {
                Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
            }
        } else if self.at_eof() {
            Err(perr(self.cur().offset, ParseKind::UnexpectedEof))
        } else {
            Err(perr(self.cur().offset, ParseKind::UnsupportedStatement))
        }
    }

    /// The savepoint and maintenance statements (v0.16, `v16`):
    /// `SAVEPOINT nm`, `RELEASE [SAVEPOINT] nm`, `VACUUM [nm]
    /// [INTO expr]`, `REINDEX [nm [. nm]]`, `ANALYZE [nm [.
    /// nm]]`, with the current token on the keyword.
    #[cfg(feature = "v16")]
    fn maint_stmt<E>(&mut self) -> Result<Ast, E> {
        if self.eat_kw(b"SAVEPOINT") {
            let name = self.pragma_nm::<E>()?;
            return Ok(Ast::Savepoint { name });
        }
        if self.eat_kw(b"RELEASE") {
            let _ = self.eat_kw(b"SAVEPOINT");
            let name = self.pragma_nm::<E>()?;
            return Ok(Ast::Release { name });
        }
        let vacuum = self.eat_kw(b"VACUUM");
        let reindex = !vacuum && self.eat_kw(b"REINDEX");
        if !vacuum && !reindex {
            self.pos += 1; // ANALYZE
        }
        let mut first = None;
        if !self.at_eof() && !matches!(self.cur().tok, Tok::Semi) && !self.cur_is_kw(b"INTO") {
            first = Some(self.pragma_nm::<E>()?);
        }
        if vacuum {
            // `VACUUM [nm] INTO expr`.
            let into = if self.eat_kw(b"INTO") {
                Some(self.item::<E>(3 + usize::from(first.is_some()))?)
            } else {
                None
            };
            return Ok(Ast::Vacuum {
                schema: first,
                into,
            });
        }
        let (schema, name) = if first.is_some() && self.eat(Tok::Dot) {
            (first, Some(self.pragma_nm::<E>()?))
        } else {
            (None, first)
        };
        Ok(if reindex {
            Ast::Reindex { schema, name }
        } else {
            Ast::Analyze { schema, name }
        })
    }

    /// `PRAGMA [nm .] nm [= nmnum | ( nmnum )]` (v0.16, `v16`;
    /// the pin's grammar): `nmnum` is a signed number, a name
    /// (an identifier that is not a reserved word, or a string),
    /// or one of `ON`, `DELETE`, `DEFAULT`.
    #[cfg(feature = "v16")]
    fn pragma_stmt<E>(&mut self) -> Result<Ast, E> {
        self.pos += 1;
        let first = self.pragma_nm::<E>()?;
        let (schema, name) = if self.eat(Tok::Dot) {
            (Some(first), self.pragma_nm::<E>()?)
        } else {
            (None, first)
        };
        let paren = matches!(self.cur().tok, Tok::LParen);
        let mut numeric = false;
        let value = if self.eat(Tok::Eq) || self.eat(Tok::LParen) {
            let i = self.pos + usize::from(matches!(self.cur().tok, Tok::Minus | Tok::Plus));
            numeric = matches!(
                self.toks.as_slice().get(i).map(|t| t.tok),
                Some(Tok::Integer(_) | Tok::Real(_))
            );
            let v = self.pragma_value::<E>()?;
            if paren {
                self.expect::<E>(Tok::RParen)?;
            }
            Some(v)
        } else {
            None
        };
        Ok(Ast::Pragma {
            schema,
            name,
            value,
            numeric,
        })
    }

    /// One PRAGMA name (`nm`): a non-reserved or quoted
    /// identifier, or a string.
    #[cfg(feature = "v16")]
    fn pragma_nm<E>(&mut self) -> Result<ABytes, E> {
        match self.cur().tok {
            Tok::Ident(h, q) if q || !self.reserved(self.pos, true) => {
                self.pos += 1;
                Ok(h)
            }
            Tok::Text(h) => {
                self.pos += 1;
                Ok(h)
            }
            Tok::Eof => Err(perr(self.cur().offset, ParseKind::UnexpectedEof)),
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    /// One PRAGMA argument (`nmnum` / `minus_num`) as text.
    #[cfg(feature = "v16")]
    fn pragma_value<E>(&mut self) -> Result<ABytes, E> {
        let minus = matches!(self.cur().tok, Tok::Minus);
        if minus || matches!(self.cur().tok, Tok::Plus) {
            self.pos += 1;
            if !matches!(self.cur().tok, Tok::Integer(_) | Tok::Real(_)) {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
        }
        match self.cur().tok {
            Tok::Integer(_) | Tok::Real(_) => {
                let start = self.cur().offset;
                let end = self.item_end_offset(self.pos + 1);
                let text = trim(self.sql.get(start..end).unwrap_or_default());
                // A digit separator makes the pin's tokenizer
                // produce a token the PRAGMA grammar refuses.
                if text.contains(&b'_') {
                    return Err(perr(start, ParseKind::UnexpectedToken));
                }
                self.pos += 1;
                let mut v: AVec<u8> = AVec::new();
                if minus {
                    v.push(b'-');
                }
                v.extend_from_slice(text);
                Ok(self.arena.alloc_copy_handle(v.as_slice())?)
            }
            Tok::Ident(h, q)
                if q || !self.reserved(self.pos, true)
                    || self.cur_is_kw(b"ON")
                    || self.cur_is_kw(b"DELETE")
                    || self.cur_is_kw(b"DEFAULT") =>
            {
                self.pos += 1;
                Ok(h)
            }
            Tok::Text(h) => {
                self.pos += 1;
                Ok(h)
            }
            Tok::Eof | Tok::Semi => Err(perr(self.cur().offset, ParseKind::UnexpectedEof)),
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
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
        #[cfg(feature = "triggers")]
        if self.is_kw(self.pos + 1, b"TRIGGER") {
            return self.drop_trigger::<E>();
        }
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
        // v0.16 (`expr_index`): every item is an expression; a
        // bare column, a lone string and either under a COLLATE
        // stay column items (the pin's `sqlite3StringToId`, then
        // `TK_COLUMN` after `sqlite3ExprSkipCollate`).
        #[cfg(feature = "expr_index")]
        loop {
            let at = self.cur().offset;
            let e = self.item::<E>(IDX_HELD)?;
            let end = self.cur().offset;
            let (inner, coll) = match &e {
                Expr::Collate(x, c) => (x.as_ref(), Some(*c)),
                x => (x, None),
            };
            let col = match inner {
                Expr::Column { table: None, name } => Some(*name),
                Expr::Literal(Literal::Text(h)) => Some(*h),
                _ => None,
            };
            let expr = match col {
                Some(_) => None,
                None => Some(self.arena.alloc_copy_handle(trim(self.span(at, end)))?),
            };
            let desc = if self.eat_kw(b"DESC") {
                true
            } else {
                let _ = self.eat_kw(b"ASC");
                false
            };
            columns.push(IndexColumnDef {
                name: col.unwrap_or(ABytes::EMPTY),
                desc,
                coll,
                expr,
            });
            if self.eat(Tok::Comma) {
                continue;
            }
            self.expect::<E>(Tok::RParen)?;
            break;
        }
        #[cfg(not(feature = "expr_index"))]
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
                #[cfg(feature = "expr_index")]
                expr: None,
            });
            if self.eat(Tok::Comma) {
                continue;
            }
            self.expect::<E>(Tok::RParen)?;
            break;
        }
        // The pin's "too many columns in index" (v0.16).
        if columns.len() > MAX_COLUMN {
            return Err(perr(self.cur().offset, ParseKind::TooManyColumns));
        }
        // A partial index's WHERE (v0.16, `expr_index`); refused
        // without it.
        #[cfg(feature = "expr_index")]
        let filter = if self.eat_kw(b"WHERE") {
            let at = self.cur().offset;
            self.item::<E>(IDX_HELD + 2)?;
            let end = self.cur().offset;
            Some(self.arena.alloc_copy_handle(trim(self.span(at, end)))?)
        } else {
            None
        };
        #[cfg(not(feature = "expr_index"))]
        if self.cur_is_kw(b"WHERE") {
            return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
        }
        Ok(Ast::CreateIndex {
            name,
            table,
            unique,
            if_not_exists,
            columns,
            #[cfg(feature = "expr_index")]
            filter,
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
            // `create_table AS select`.
            let select = self.select_at::<E>(3)?;
            return Ok(Ast::CreateTable {
                name,
                if_not_exists,
                columns: AVec::new(),
                pk_columns: AVec::new(),
                constraint_sets: AVec::new(),
                checks: AVec::new(),
                strict: false,
                #[cfg(feature = "without_rowid")]
                without_rowid: false,
                as_select: Some(Box::new(select)),
                fkeys: AVec::new(),
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
        #[cfg(feature = "v16")]
        let mut fkeys: AVec<crate::sql::ast::ForeignKey> = AVec::new();
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
                #[cfg(feature = "v16")]
                &mut fkeys,
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
        // The pin's "too many columns on t" (v0.16 fix, ungated: a
        // table of more than 2000 columns made a file the pin
        // cannot open).
        if columns.len() > MAX_COLUMN {
            return Err(perr(self.cur().offset, ParseKind::TooManyColumns));
        }
        // A PRIMARY KEY / UNIQUE list names the table's columns
        // (v0.16 fix: `PRIMARY KEY (nosuch)` was written, and the
        // pin then could not read the schema). The tiny profile
        // keeps no constraint indexes at all (SPEC §2), so the
        // check lives with them.
        #[cfg(feature = "indexes")]
        for s in sets.iter() {
            self.known_columns::<E>(columns.as_slice(), s.columns.as_slice())?;
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
        // Table options (v0.16, `v16`): a comma-separated list of
        // STRICT and WITHOUT ROWID, as on the pin (`STRICT,STRICT`
        // parses; `STRICT STRICT` does not). WITHOUT ROWID stays
        // outside every hipp phase and is refused.
        #[cfg(feature = "v16")]
        let mut strict = false;
        #[cfg(feature = "without_rowid")]
        let mut without_rowid = false;
        #[cfg(feature = "v16")]
        if self.cur_is_kw(b"STRICT") || self.cur_is_kw(b"WITHOUT") {
            loop {
                if self.cur_is_kw(b"WITHOUT") {
                    // `WITHOUT <name>` (v0.16, `without_rowid`):
                    // ROWID in any case; any other name is the
                    // pin's "unknown table option".
                    #[cfg(feature = "without_rowid")]
                    {
                        self.pos += 1;
                        if !self.cur_is_kw(b"ROWID") {
                            return Err(perr(self.cur().offset, ParseKind::UnknownTableOption));
                        }
                        self.pos += 1;
                        without_rowid = true;
                    }
                    #[cfg(not(feature = "without_rowid"))]
                    return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
                } else {
                    self.expect_kw::<E>(b"STRICT")?;
                    strict = true;
                }
                if !self.eat(Tok::Comma) {
                    break;
                }
            }
        }
        // A WITHOUT ROWID table needs a PRIMARY KEY (the pin's
        // "PRIMARY KEY missing on table t").
        #[cfg(feature = "without_rowid")]
        if without_rowid && pk_sets == 0 {
            return Err(perr(self.cur().offset, ParseKind::PrimaryKeyMissing));
        }
        #[cfg(not(feature = "without_rowid"))]
        let without_rowid = false;
        #[cfg(feature = "indexes")]
        let sets = self.dedupe_sets(sets, &columns, &pk_columns, without_rowid);
        #[cfg(not(feature = "indexes"))]
        let _ = without_rowid;
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
            #[cfg(feature = "without_rowid")]
            without_rowid,
            #[cfg(feature = "v16")]
            as_select: None,
            #[cfg(feature = "v16")]
            fkeys,
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
        without_rowid: bool,
    ) -> AVec<ConstraintSet> {
        let name = |h: ABytes| handle_bytes(self.arena, h);
        let col = |h: ABytes| columns.iter().find(|c| eq_ci(name(c.name), name(h)));
        let alias = !without_rowid
            && pk_columns.len() == 1
            && pk_columns
                .as_slice()
                .first()
                .and_then(|h| col(*h))
                .is_some_and(|c| {
                    !c.pk_desc
                        && eq_ci(
                            crate::record::decl_token(handle_bytes(self.arena, c.decl)),
                            b"INTEGER",
                        )
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
        let text = trim(self.span(start.min(end), end));
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

    /// A literal at token `i` under any run of unary signs:
    /// the literal and the tokens it spans. Walked without
    /// recursion (v0.16 fix, ungated: a recursive walk per sign
    /// overflowed the stack on `INSERT ... VALUES (- - - ... 1)`,
    /// the tiny profile's grammar included).
    fn literal_at(&self, i: usize) -> Option<(Literal, usize)> {
        let toks = self.toks.as_slice().get(..self.limit)?;
        // The signs, then the literal they apply to.
        let mut j = i;
        while matches!(toks.get(j).map(|t| t.tok), Some(Tok::Minus | Tok::Plus)) {
            j += 1;
        }
        let t = toks.get(j)?;
        let mut lit = match t.tok {
            Tok::Integer(v) => Literal::Integer(v),
            Tok::Real(f) => Literal::Real(f),
            Tok::Text(h) => Literal::Text(h),
            Tok::Blob(h) => Literal::Blob(h),
            Tok::Ident(h, false) => {
                let b = handle_bytes(self.arena, h);
                if eq_ci(b, b"NULL") {
                    Literal::Null
                } else if eq_ci(b, b"TRUE") {
                    Literal::Integer(1)
                } else if eq_ci(b, b"FALSE") {
                    Literal::Integer(0)
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        // Apply the signs from the innermost out. The innermost
        // `-` (only `+` between it and the literal) follows the
        // pin's `codeInteger`: over the integer literal
        // 9223372036854775808 it is INTEGER i64::MIN, not a REAL
        // (v0.16); over i64::MIN itself it overflows to REAL; and
        // `-0x8000000000000000` is the pin's "hex literal too big",
        // not a REAL (v0.16 fix): no literal here, so the
        // evaluator's unary minus reports it. (The tiny lexer
        // refuses the literal itself.) A `+` leaves the value.
        let mut innermost = true;
        let mut k = j;
        while k > i {
            k -= 1;
            if !matches!(toks.get(k).map(|t| t.tok), Some(Tok::Minus)) {
                continue;
            }
            if innermost && self.min_magnitude(j) {
                lit = Literal::Integer(i64::MIN);
            } else {
                #[cfg(feature = "v04")]
                if innermost && self.hex_min(j) {
                    return None;
                }
                lit = match lit {
                    Literal::Integer(v) => v
                        .checked_neg()
                        .map_or(Literal::Real(-(v as f64)), Literal::Integer),
                    Literal::Real(f) => Literal::Real(-f),
                    _ => return None,
                };
            }
            innermost = false;
        }
        Some((lit, j - i + 1))
    }

    /// The pin's limits on a literal under a run of unary signs at
    /// token `i`, spanning `used` tokens (`literal_at`), when the
    /// grammar folds it without parsing it as an expression (v0.16:
    /// the tiny grammar's literals, a parenthesised DEFAULT, a
    /// LIMIT without `v16`): one parser-stack symbol per token over
    /// `held` ("Recursion limit"), and the tree height of its `+` /
    /// `-` nodes, `nodes` more sitting above them — a `+` over a `+`
    /// node re-uses it, a `-` over one turns it into a `-` node
    /// ("Expression tree is too large").
    fn sign_run<E>(&self, i: usize, used: usize, held: usize, nodes: usize) -> Result<(), E> {
        let run = self
            .toks
            .as_slice()
            .get(i..i.saturating_add(used))
            .unwrap_or(&[]);
        // A sign makes a node unless the one inside it is a `+`.
        let h = run
            .windows(2)
            .filter(|w| !matches!(w.last().map(|t| t.tok), Some(Tok::Plus)))
            .count()
            .saturating_add(nodes)
            .saturating_add(1);
        let kind = if held.saturating_add(used) > self.lim.stack() {
            ParseKind::RecursionLimit
        } else if h > self.lim.expr() {
            ParseKind::ExprTooDeep
        } else {
            return Ok(());
        };
        Err(perr(run.first().map_or(0, |t| t.offset), kind))
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
            let returning = self.returning_clause::<E>(INS_RETURNING)?;
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
        if self.cur_is_kw(b"SELECT") || self.with_at(self.pos) {
            return Ok(InsertSource {
                rows: AVec::new(),
                select: Some(Box::new(self.select_at::<E>(INS_BASE)?)),
            });
        }
        Ok(InsertSource {
            rows: self.values_rows::<E>(INS_BASE, true)?,
            select: None,
        })
    }

    #[cfg(not(feature = "v08"))]
    fn insert_source<E>(&mut self) -> Result<InsertSource, E> {
        Ok(InsertSource {
            rows: self.values_rows::<E>(INS_BASE, true)?,
            select: None,
        })
    }

    /// The `VALUES` row list: v0.2 one or more parenthesised
    /// rows, comma-separated; v0.4 each value is an expression
    /// (a plain literal under the pre-v0.4 grammar). `base`
    /// symbols sit below it on the pin's parser stack.
    ///
    /// A row of more than 2000 values is the pin's "too many columns
    /// in result set" where it reads the rows as a select: a VALUES
    /// core it resolves (not in a CREATE VIEW or CTE body, `defer`),
    /// and an INSERT's rows when there are several (`insert`; one
    /// row is not a select there — `INSERT INTO t(a, a, ...)` takes
    /// any number).
    fn values_rows<E>(&mut self, base: usize, insert: bool) -> Result<AVec<AVec<Expr>>, E> {
        self.expect_kw::<E>(b"VALUES")?;
        let mut rows = AVec::new();
        #[cfg(feature = "v04")]
        let mut core_h = 1usize;
        loop {
            self.expect::<E>(Tok::LParen)?;
            // `VALUES LP` / `values COMMA LP`, then `nexprlist
            // COMMA` before a later value.
            let lp = if rows.is_empty() { 2 } else { 3 };
            let mut row = AVec::new();
            loop {
                let at = base + lp + if row.is_empty() { 0 } else { 2 };
                row.push(self.value_expr::<E>(at)?);
                #[cfg(feature = "v04")]
                {
                    core_h = core_h.max(self.eh);
                }
                if self.eat(Tok::Comma) {
                    continue;
                }
                self.expect::<E>(Tok::RParen)?;
                break;
            }
            #[cfg(feature = "v04")]
            self.peak::<E>(base + lp + 2)?;
            rows.push(row);
            if self.eat(Tok::Comma) {
                continue;
            }
            break;
        }
        #[cfg(feature = "v04")]
        {
            self.core_h = core_h;
            let wide = rows.iter().any(|r| r.len() > MAX_COLUMN);
            if wide && if insert { rows.len() > 1 } else { !self.defer } {
                return Err(perr(self.cur().offset, ParseKind::TooManyColumns));
            }
        }
        #[cfg(not(feature = "v04"))]
        let _ = insert;
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
        } else if self.eat_kw(b"FAIL") {
            Ok(ConflictAction::Fail)
        } else if self.eat_kw(b"ROLLBACK") {
            #[cfg(feature = "v16")]
            return Ok(ConflictAction::Rollback);
            #[cfg(not(feature = "v16"))]
            return Ok(ConflictAction::Abort);
        } else if self.eat_kw(b"ABORT") {
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
        // `... select [upsert] RETURNING`: the pin's RETURNING is
        // the tail of its upsert rule, above that clause's symbols
        // (`ON CONFLICT [LP sortlist RP where_opt] DO NOTHING |
        // UPDATE SET setlist where_opt`).
        let held = match &upsert {
            None => INS_RETURNING,
            Some(u) => {
                let target = !u.target.is_empty();
                let nothing = matches!(u.action, UpsertAction::Nothing);
                INS_RETURNING + 3 + if target { 4 } else { 0 } + if nothing { 1 } else { 4 }
            }
        };
        let returning = self.returning_clause::<E>(held)?;
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
        #[cfg(feature = "expr_index")]
        let mut target_exprs = AVec::new();
        // v0.16 (`expr_index`): `indexed-column` items are
        // expressions; a bare column (under an optional COLLATE)
        // names a column as before.
        #[cfg(feature = "expr_index")]
        if self.eat(Tok::LParen) {
            loop {
                let e = self.item::<E>(INS_BASE + 9)?;
                let (inner, coll) = match e {
                    Expr::Collate(x, c) => (*x, Some(c)),
                    x => (x, None),
                };
                match inner {
                    Expr::Column { table: None, name } => {
                        target.push(name);
                        target_exprs.push(None);
                    }
                    x => {
                        target.push(ABytes::EMPTY);
                        target_exprs.push(Some(x));
                    }
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
        #[cfg(not(feature = "expr_index"))]
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
        // `with insert_cmd INTO xfullname idlist_opt select ON
        // CONFLICT LP sortlist RP WHERE expr`.
        let target_filter = if self.eat_kw(b"WHERE") {
            Some(self.item::<E>(INS_BASE + 7)?)
        } else {
            None
        };
        self.expect_kw::<E>(b"DO")?;
        let action = if self.eat_kw(b"NOTHING") {
            UpsertAction::Nothing
        } else {
            self.expect_kw::<E>(b"UPDATE")?;
            self.expect_kw::<E>(b"SET")?;
            // `... ON CONFLICT [LP sortlist RP where_opt] DO UPDATE
            // SET nm EQ expr ... [WHERE expr]`.
            let at = INS_BASE + if target.is_empty() { 8 } else { 12 };
            let assignments = self.set_list::<E>(at)?;
            let filter = if self.eat_kw(b"WHERE") {
                Some(self.item::<E>(at)?)
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
            #[cfg(feature = "expr_index")]
            target_exprs,
            target_filter,
            action,
        })
    }

    /// An optional `RETURNING <items>` clause (v0.7), shared by
    /// INSERT / UPDATE / DELETE: SELECT-shaped result items
    /// (`*`, `t.*`, or an expression with an optional alias).
    /// Returns an empty vec when the clause is absent.
    ///
    /// `held` symbols sit below an item on the pin's parser stack
    /// (`RETURNING sclp scanpt`).
    #[cfg(feature = "v07")]
    fn returning_clause<E>(&mut self, held: usize) -> Result<AVec<SelectItem>, E> {
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
                let e = self.item::<E>(held)?;
                let end = self.cur().offset;
                let text = self.arena.alloc_copy_handle(trim(self.span(start, end)))?;
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
    /// `held` symbols sit below it on the pin's parser stack.
    #[cfg(feature = "v04")]
    fn value_expr<E>(&mut self, held: usize) -> Result<Expr, E> {
        self.item::<E>(held)
    }

    #[cfg(not(feature = "v04"))]
    fn value_expr<E>(&mut self, held: usize) -> Result<Expr, E> {
        #[cfg(feature = "params")]
        if let Some(e) = self.param_here() {
            return Ok(e);
        }
        let lit = self
            .signed_literal::<E>(held, 0)?
            .ok_or(perr(self.cur().offset, ParseKind::UnexpectedToken))?;
        Ok(Expr::Literal(lit))
    }

    /// A literal under any run of unary signs, consumed, with the
    /// pin's limits on the run (v0.16, the tiny grammar; the v0.4
    /// grammar counts them in `expr_unary`): one parser-stack
    /// symbol per sign over `held` ("Recursion limit"), and the
    /// tree height of its `+` / `-` nodes — a `+` over a `+` node
    /// re-uses it, a `-` over one turns it into a `-` node
    /// ("Expression tree is too large"). `None` (nothing consumed)
    /// when no literal starts here.
    #[cfg(not(feature = "v04"))]
    ///
    /// `nodes` more nodes sit above the run in the pin's tree (the
    /// `=` of `WHERE col = literal`).
    fn signed_literal<E>(&mut self, held: usize, nodes: usize) -> Result<Option<Literal>, E> {
        let Some((lit, used)) = self.literal_at(self.pos) else {
            return Ok(None);
        };
        self.sign_run::<E>(self.pos, used, held, nodes)?;
        self.pos += used;
        Ok(Some(lit))
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
        // The pin's "too many columns in result set" (v0.16 fix).
        if items.len() > MAX_COLUMN {
            return Err(perr(self.cur().offset, ParseKind::TooManyColumns));
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
            #[cfg(feature = "v16")]
            args: None,
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
            // `SELECT distinct selcollist from WHERE expr EQ`.
            #[cfg(feature = "params")]
            let rhs = match self.param_here() {
                Some(e) => e,
                None => Expr::Literal(
                    self.signed_literal::<E>(8, 1)?
                        .ok_or(perr(self.cur().offset, ParseKind::UnsupportedClause))?,
                ),
            };
            #[cfg(not(feature = "params"))]
            let rhs = Expr::Literal(
                self.signed_literal::<E>(8, 1)?
                    .ok_or(perr(self.cur().offset, ParseKind::UnsupportedClause))?,
            );
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
                KWS.iter().any(|k| eq_ci(b, k))
                    || (cfg!(feature = "v07") && eq_ci(b, b"RETURNING"))
                    || (cfg!(feature = "window") && self.window_clause_at(self.pos))
            }
            _ => false,
        }
    }

    /// Whether token `i` starts a `WINDOW name AS` clause (v0.16,
    /// `window`): the pin's tokenizer reads WINDOW as a keyword
    /// only when an identifier (or string) and AS follow it
    /// (`analyzeWindowKeyword`), so `SELECT 1 window` stays an
    /// alias.
    fn window_clause_at(&self, i: usize) -> bool {
        cfg!(feature = "window")
            && self.is_kw(i, b"WINDOW")
            && matches!(
                self.toks.as_slice().get(i + 1).map(|t| t.tok),
                Some(Tok::Ident(..) | Tok::Text(_))
            )
            && self.is_kw(i + 2, b"AS")
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
                Tok::Ident(h, quoted) if quoted || !self.reserved(self.pos, true) => {
                    self.pos += 1;
                    Some(h)
                }
                // `AS` with no name after it is the pin's syntax
                // error: leave it for the caller to refuse (v0.16
                // review fix; `SELECT 1 AS` returned 1).
                _ => {
                    self.pos = self.pos.saturating_sub(1);
                    None
                }
            };
        }
        match self.cur().tok {
            // A bare alias is the pin's `ids`: never a reserved
            // keyword (v0.16 review fix: `SELECT 1 NOT` read NOT
            // as an alias where the pin gives a syntax error).
            Tok::Ident(h, quoted) => {
                if quoted || !(self.at_clause_kw() || self.reserved(self.pos, false)) {
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

    // ---------- the pin's nesting limits (v0.16) ----------
    // Two counts follow the pin's parser as it reads the statement,
    // so hipp refuses exactly what it refuses — and every recursive
    // walk of an accepted statement (this parser, the resolver, the
    // evaluator, the derived Clone / Drop) is bounded by them:
    //
    // - `held`: the symbols on the pin's LALR stack below the
    //   construct being parsed. Each construct adds the symbols its
    //   grammar rule holds while a nested part is read (`expr +` is
    //   two, `CASE case_operand WHEN` three, ...) and `need` checks
    //   the peak a construct reaches against `Limits::parser_depth`
    //   ("Recursion limit").
    // - `eh` / `ek`: the height the pin gives the expression just
    //   parsed (`Expr.nHeight`: a leaf is 1, a node one more than
    //   its tallest operand; parentheses and a row value's
    //   elements add nothing, `+` over a `+` node re-uses it, `x
    //   ISNULL` over a literal folds to one), checked against
    //   `Limits::expr_depth` as each node is made ("Expression tree
    //   is too large"). Name resolution adds the heights of the
    //   expressions a subquery sits in (`Parse.nHeight`): `rsub`
    //   and `sel_r` carry that sum up (`resolved`).

    /// The pin's stack would hold `k` more symbols here.
    fn need<E>(&self, k: usize) -> Result<(), E> {
        self.peak::<E>(self.held.saturating_add(k))
    }

    /// The pin's stack would hold `abs` symbols in all here.
    fn peak<E>(&self, abs: usize) -> Result<(), E> {
        if abs > self.lim.parser_depth {
            return Err(perr(self.cur().offset, ParseKind::RecursionLimit));
        }
        Ok(())
    }

    /// Record the height and kind of the expression just made.
    fn set_h<E>(&mut self, h: usize, k: u8) -> Result<(), E> {
        if h > self.lim.expr_depth {
            return Err(perr(self.cur().offset, ParseKind::ExprTooDeep));
        }
        self.eh = h;
        self.ek = k;
        Ok(())
    }

    /// A node over the expression just parsed (and over operands
    /// of heights up to `other`, kinds `ok`): one taller.
    fn node<E>(&mut self, other: usize, ok: u8) -> Result<(), E> {
        let h = self.eh.max(other).saturating_add(1);
        let k = self.ek & ok & EK_CONST;
        self.set_h::<E>(h, k)
    }

    /// An operand parsed with `k` more symbols on the pin's stack,
    /// at binding power `min`.
    fn operand<E>(&mut self, k: usize, min: u8) -> Result<Expr, E> {
        let h0 = self.held;
        self.held = h0.saturating_add(k);
        self.peak::<E>(self.held)?;
        let r = self.expr_bp::<E>(min);
        self.held = h0;
        r
    }

    /// A whole expression parsed with `k` more symbols on the
    /// pin's stack.
    fn operand_expr<E>(&mut self, k: usize) -> Result<Expr, E> {
        self.operand::<E>(k, BP_OR)
    }

    /// The resolution depth of the expression just parsed: its
    /// height plus the deepest resolution depth of a subquery in
    /// it — checked, as the pin's `sqlite3ResolveExprNames`
    /// checks `Parse.nHeight`.
    fn resolved<E>(&self) -> Result<usize, E> {
        let h = if self.reduced && self.eh == 1 {
            0
        } else {
            self.eh
        };
        let r = h.saturating_add(self.rsub);
        if r > self.lim.expr_depth && !self.defer {
            return Err(perr(self.cur().offset, ParseKind::ExprTooDeep));
        }
        Ok(r)
    }

    /// One expression the pin resolves on its own (a result
    /// column, WHERE, a SET value, ...), with `held` symbols below
    /// it: parsed, its resolution depth checked and folded into
    /// the current select's (`sel_*` accumulators `acc_h` /
    /// `acc_r`). Returns the expression; `eh` / `rsub` stay set
    /// for the caller.
    fn item<E>(&mut self, held: usize) -> Result<Expr, E> {
        #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
        let mut e = self.raw::<E>(held)?;
        self.fold_item::<E>(true)?;
        #[cfg(feature = "v16")]
        mark_ctx(&mut e, self.eh);
        Ok(e)
    }

    /// Fold the expression just parsed (`raw`) into the current
    /// select: its height always (`heightOfSelect`), its
    /// resolution depth when the pin resolves it (`resolve`).
    fn fold_item<E>(&mut self, resolve: bool) -> Result<(), E> {
        self.acc_h = self.acc_h.max(self.eh);
        if resolve {
            let depth = self.resolved::<E>()?;
            self.acc_r = self.acc_r.max(depth);
        }
        Ok(())
    }

    /// An ORDER BY / GROUP BY term with `held` symbols below it:
    /// an item, except that an integer term is a column ordinal the
    /// pin never resolves.
    ///
    /// The term is an ordinal when the pin's `sqlite3ExprIsInteger`
    /// says so (v0.16 review fix, ungated): under any COLLATE, any
    /// run of unary `+` / `-` and parentheses over a 32-bit integer
    /// literal (or a null test folded to one), so `-(-2)` and `+(2)`
    /// are the ordinal 2 and `-(2)` the out-of-range -2; the term is
    /// then made that integer literal. TRUE / FALSE are not integers
    /// there (`ORDER BY TRUE` sorts by a constant): such a term is
    /// kept from reading as an ordinal.
    fn sort_item<E>(&mut self, held: usize) -> Result<Expr, E> {
        let p0 = self.pos;
        let mut e = self.raw::<E>(held)?;
        let word = self
            .toks
            .as_slice()
            .get(p0..self.pos)
            .unwrap_or(&[])
            .iter()
            .any(|t| match t.tok {
                Tok::Ident(h, false) => {
                    let b = handle_bytes(self.arena, h);
                    eq_ci(b, b"TRUE") || eq_ci(b, b"FALSE")
                }
                _ => false,
            });
        ordinal_term(&mut e, word);
        self.fold_item::<E>(!matches!(e, Expr::Literal(Literal::Integer(_))))?;
        #[cfg(feature = "v16")]
        mark_ctx(&mut e, self.eh);
        Ok(e)
    }

    /// An expression with `held` symbols below it, its subqueries'
    /// resolution depths collected afresh in `rsub` (left set for
    /// the caller, as is `eh`).
    fn raw<E>(&mut self, held: usize) -> Result<Expr, E> {
        let h0 = self.held;
        self.held = held;
        self.peak::<E>(self.held)?;
        self.rsub = 0;
        let r = self.parse_expr::<E>();
        self.held = h0;
        r
    }

    /// A subquery's select statement with `k` more symbols below
    /// it; afterwards `sel_h` / `sel_r` describe it and `rsub`
    /// includes its resolution depth.
    #[cfg(feature = "v06")]
    fn subquery<E>(&mut self, k: usize) -> Result<SelectStmt, E> {
        let h0 = self.held;
        self.held = h0.saturating_add(k);
        self.peak::<E>(self.held)?;
        let r = self.select_rich::<E>();
        self.held = h0;
        let s = r?;
        self.rsub = self.rsub.max(self.sel_r);
        Ok(s)
    }

    /// An expression whose operators all bind at least as
    /// tightly as `min`.
    fn expr_bp<E>(&mut self, min: u8) -> Result<Expr, E> {
        let p0 = self.pos;
        let mut l = self.expr_prefix::<E>()?;
        while let Some(bp) = self.infix_bp() {
            if bp < min {
                break;
            }
            if bp == BP_OR || bp == BP_AND {
                self.mark_and_or_operand(&mut l, p0);
            }
            l = self.expr_infix::<E>(l, bp)?;
        }
        Ok(l)
    }

    /// An integer-literal operand of AND / OR (tokens `p0` to the
    /// cursor) that the pin does not treat as a constant truth
    /// value is wrapped in a unary `+` (v0.16 review fix, ungated):
    /// the pin simplifies `0 AND x` / `x OR 1` only over a single
    /// integer token holding a 32-bit value (or TRUE / FALSE), so
    /// `-1 OR x`, `+0 AND x`, `1_0 OR x` and `2147483648 OR x` still
    /// evaluate `x` there (the evaluator's short-circuit, `int_lit`,
    /// sees only a bare literal).
    fn mark_and_or_operand(&self, e: &mut Expr, p0: usize) {
        if !matches!(e, Expr::Literal(Literal::Integer(_))) {
            return;
        }
        let toks = self.toks.as_slice().get(p0..self.pos).unwrap_or(&[]);
        let mut a = 0;
        let mut b = toks.len();
        while b > a + 1
            && matches!(toks.get(a).map(|t| t.tok), Some(Tok::LParen))
            && matches!(toks.get(b - 1).map(|t| t.tok), Some(Tok::RParen))
        {
            a += 1;
            b -= 1;
        }
        let simple = b == a + 1
            && toks.get(a).is_some_and(|t| match t.tok {
                Tok::Integer(v) => {
                    (0..=i64::from(i32::MAX)).contains(&v)
                        && !self
                            .sql
                            .get(t.offset..)
                            .unwrap_or(&[])
                            .iter()
                            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
                            .any(|c| *c == b'_')
                }
                Tok::Ident(h, false) => {
                    let w = handle_bytes(self.arena, h);
                    eq_ci(w, b"TRUE") || eq_ci(w, b"FALSE")
                }
                _ => false,
            });
        if !simple {
            let inner = core::mem::replace(e, Expr::Literal(Literal::Null));
            *e = Expr::Pos(Box::new(inner));
        }
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
            #[cfg(feature = "json")]
            Tok::Ptr(_) => BP_CONCAT,
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
        let (lh, lk) = (self.eh, self.ek);
        if bp == BP_OR || bp == BP_AND {
            self.pos += 1;
            let r0 = self.pos;
            // `expr AND expr`: two symbols below the right operand.
            let mut r = self.operand::<E>(2, bp + 1)?;
            self.mark_and_or_operand(&mut r, r0);
            self.node::<E>(lh, lk)?;
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
            // `expr COLLATE ids`.
            self.need::<E>(3)?;
            self.node::<E>(0, EK_CONST)?;
            self.ek |= EK_COLLATE;
            return Ok(Expr::Collate(Box::new(l), c));
        }
        // `expr -> expr` / `expr ->> expr` (v0.16, `json`): the pin
        // parses them as calls of its functions `->` / `->>`.
        #[cfg(feature = "json")]
        if let Tok::Ptr(long) = self.cur().tok {
            self.pos += 1;
            let r = self.operand::<E>(2, bp + 1)?;
            self.node::<E>(lh, lk)?;
            let mut args = AVec::new();
            args.push(l);
            args.push(r);
            let name: &[u8] = if long { b"->>" } else { b"->" };
            return Ok(Expr::Func {
                name: self.arena.alloc_copy_handle(name)?,
                distinct: false,
                star: false,
                args,
                clause: None,
            });
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
        // `==` is spelled with two Eq tokens by the lexer (one
        // token, one symbol, on the pin).
        if op == BinOp::Eq {
            let _ = self.eat(Tok::Eq);
        }
        let r = self.operand::<E>(2, bp + 1)?;
        self.node::<E>(lh, lk)?;
        Ok(bin(op, l, r))
    }

    /// `x ISNULL` / `NOTNULL` / `NOT NULL` / `IS [NOT] NULL` over
    /// the expression just parsed: the pin folds it to a literal
    /// when the operand is a numeric, string or blob literal
    /// (under any unary `+` / `-`), else a node.
    ///
    /// The fold is the pin's `sqlite3PExprIsNull`: the test becomes
    /// the integer literal `notnull` (v0.16 fix, ungated: so `ORDER
    /// BY 2 NOTNULL` is the ordinal 1 and `GROUP BY 1 ISNULL` the
    /// out-of-range ordinal 0, as on the pin — hipp sorted and
    /// grouped by a constant). Returns the expression made.
    fn null_test<E>(&mut self, operand: Expr, notnull: bool) -> Result<Expr, E> {
        if self.ek & EK_LIT != 0 {
            self.set_h::<E>(1, EK_LIT | EK_CONST)?;
            return Ok(Expr::Literal(Literal::Integer(i64::from(notnull))));
        }
        self.node::<E>(0, EK_CONST)?;
        Ok(Expr::IsNull(Box::new(operand), notnull))
    }

    /// The keyword operators of the `=` level: IS [NOT]
    /// [DISTINCT FROM], ISNULL, NOTNULL, NOT NULL, [NOT] IN /
    /// LIKE / GLOB / BETWEEN.
    fn expr_eq_kw<E>(&mut self, l: Expr) -> Result<Expr, E> {
        let (lh, lk) = (self.eh, self.ek);
        // Postfix ISNULL / NOTNULL (`expr ISNULL`).
        if self.cur_is_kw(b"ISNULL") || self.cur_is_kw(b"NOTNULL") {
            let neg = self.cur_is_kw(b"NOTNULL");
            self.pos += 1;
            self.need::<E>(2)?;
            return self.null_test::<E>(l, neg);
        }
        if self.eat_kw(b"IS") {
            let neg = self.eat_kw(b"NOT");
            // `IS [NOT] DISTINCT FROM` (v0.16, `v16`): `IS NOT
            // DISTINCT FROM` is IS, `IS DISTINCT FROM` is IS NOT.
            #[cfg(feature = "v16")]
            let (neg, distinct) = if self.eat_kw(b"DISTINCT") {
                self.expect_kw::<E>(b"FROM")?;
                (!neg, true)
            } else {
                (neg, false)
            };
            #[cfg(not(feature = "v16"))]
            let distinct = false;
            // `expr IS [NOT] [DISTINCT FROM] expr`.
            let k = 2 + usize::from(neg != distinct) + 2 * usize::from(distinct);
            let start = self.pos;
            let r = self.operand::<E>(k, BP_REL)?;
            let bare = |p: &Self, w: &[u8]| p.pos == start + 1 && p.is_kw(start, w);
            // `x IS [NOT] NULL` is the pin's null test (folded
            // over a literal).
            if !distinct && bare(self, b"NULL") {
                self.eh = lh;
                self.ek = lk;
                return self.null_test::<E>(l, neg);
            }
            self.node::<E>(lh, lk)?;
            // `x IS [NOT] TRUE / FALSE` with the bare keyword as
            // the whole right operand is the pin's truth test
            // (`TK_TRUTH`): any true value IS TRUE, NULL is
            // neither (v0.16 fix, ungated: `2 IS TRUE` was 0).
            // Written as the CASE it equals.
            if let Expr::Literal(Literal::Integer(v @ (0 | 1))) = r
                && (bare(self, b"TRUE") || bare(self, b"FALSE"))
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
            // `expr NOT NULL`.
            self.need::<E>(3)?;
            return self.null_test::<E>(l, true);
        }
        if self.eat_kw(b"IN") {
            return self.expr_in::<E>(l, neg);
        }
        if self.eat_kw(b"LIKE") {
            // `expr likeop expr [ESCAPE expr]`: the pin's
            // `like(pattern, x[, escape])` function node.
            let r = self.operand::<E>(2, BP_REL)?;
            // `x LIKE p ESCAPE e` (v0.16, `v16`) is the pin's
            // `like(p, x, e)` call, negated for NOT LIKE.
            #[cfg(feature = "v16")]
            if self.eat_kw(b"ESCAPE") {
                let (rh, rk) = (self.eh, self.ek);
                let esc = self.operand::<E>(4, BP_REL)?;
                self.node::<E>(lh.max(rh), lk & rk)?;
                self.negated::<E>(neg)?;
                return self.pattern_call::<E>(b"like", r, l, Some(esc), neg);
            }
            self.node::<E>(lh, lk)?;
            self.negated::<E>(neg)?;
            return Ok(Expr::Like(Box::new(l), Box::new(r), neg));
        }
        // `x [NOT] GLOB p` (v0.16, `v16`): the pin's
        // `glob(p, x)` call (no ESCAPE form).
        #[cfg(feature = "v16")]
        if self.eat_kw(b"GLOB") {
            let r = self.operand::<E>(2, BP_REL)?;
            self.node::<E>(lh, lk)?;
            self.negated::<E>(neg)?;
            return self.pattern_call::<E>(b"glob", r, l, None, neg);
        }
        if self.eat_kw(b"BETWEEN") {
            // The lower bound runs to the BETWEEN's own AND; the
            // upper bound takes only operators tighter than `=`
            // (`expr between_op expr AND expr`).
            let lo = self.operand::<E>(2, BP_NOT)?;
            let (loh, lok) = (self.eh, self.ek);
            self.expect_kw::<E>(b"AND")?;
            let hi = self.operand::<E>(4, BP_REL)?;
            self.node::<E>(lh.max(loh), lk & lok)?;
            self.negated::<E>(neg)?;
            return Ok(Expr::Between(Box::new(l), Box::new(lo), Box::new(hi), neg));
        }
        Err(perr(self.cur().offset, ParseKind::UnexpectedToken))
    }

    /// The pin's NOT node over a negated operator (`NOT LIKE`,
    /// `NOT BETWEEN`, `NOT IN`, ...).
    fn negated<E>(&mut self, neg: bool) -> Result<(), E> {
        if neg {
            self.node::<E>(0, EK_CONST)?;
        }
        Ok(())
    }

    /// The right side of `x [NOT] IN`: a table name (`v16`), a
    /// parenthesised subquery, or an expression list.
    fn expr_in<E>(&mut self, l: Expr, neg: bool) -> Result<Expr, E> {
        let (lh, lk) = (self.eh, self.ek);
        // `x IN tablename` (v0.16, `v16`): the pin reads it as
        // `x IN (SELECT * FROM tablename)` — a one-column table
        // is required at plan time.
        #[cfg(feature = "v16")]
        if let Tok::Ident(..) = self.cur().tok {
            let (table, qualified) = self.name_q::<E>()?;
            // `expr in_op nm dbnm paren_exprlist`; the subquery's
            // one result column (`*`) is a leaf.
            self.need::<E>(5)?;
            self.rsub = self.rsub.max(1);
            self.set_h::<E>(lh.max(1) + 1, 0)?;
            self.negated::<E>(neg)?;
            // A copy, not the token's handle: the pin's ALTER TABLE
            // RENAME leaves this name unedited (its "after rename"
            // check then fails), and ALTER maps token handles only.
            let table = self
                .arena
                .alloc_copy_handle(handle_bytes(self.arena, table))?;
            return Ok(Expr::InSubquery(
                Box::new(l),
                Box::new(self.table_select(table, qualified)),
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
        let is_subquery = self.cur_is_kw(b"SELECT")
            || (cfg!(feature = "v09") && self.cur_is_kw(b"VALUES"))
            || self.with_at(self.pos);
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
                // `expr in_op LP select RP` (each further `(` one
                // more symbol: a scalar-subquery list item the pin
                // turns into this form).
                let rsub0 = self.rsub;
                let stmt = self.subquery::<E>(3 + extra_parens)?;
                for _ in 0..extra_parens {
                    self.expect::<E>(Tok::RParen)?;
                }
                if self.eat(Tok::RParen) {
                    self.need::<E>(5)?;
                    self.set_h::<E>(lh.max(self.sel_h) + 1, 0)?;
                    self.negated::<E>(neg)?;
                    return Ok(Expr::InSubquery(Box::new(l), Box::new(stmt), neg));
                }
                // `IN ((SELECT ...), 2)` / `IN ((SELECT ...) + 1)`:
                // a list whose first item is a scalar subquery
                // (v0.16 review fix: a syntax error on hipp).
                if extra_parens == 0 {
                    return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
                }
                self.pos = save;
                self.rsub = rsub0;
            }
            #[cfg(not(feature = "v06"))]
            {
                return Err(perr(self.cur().offset, ParseKind::UnsupportedClause));
            }
        }
        let mut list = AVec::new();
        // The tallest item, whether all are constant, and the
        // first item's height and kind.
        let (mut ih, mut ik, mut first) = (0usize, EK_CONST, (0usize, 0u8));
        if !matches!(self.cur().tok, Tok::RParen) {
            loop {
                // `expr in_op LP [nexprlist COMMA] expr`.
                let k = if list.is_empty() { 3 } else { 5 };
                list.push(self.operand_expr::<E>(k)?);
                if list.len() == 1 {
                    first = (self.eh, self.ek);
                }
                ih = ih.max(self.eh);
                ik &= self.ek;
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect::<E>(Tok::RParen)?;
        self.need::<E>(5)?;
        if list.is_empty() {
            // The pin folds `x IN ()` to the integer 0 (1 for NOT
            // IN), dropping x.
            self.set_h::<E>(1, EK_LIT | EK_CONST)?;
            return Ok(Expr::InList(Box::new(l), list, neg));
        }
        if list.len() == 1 && first.1 & EK_CONST != 0 && first.1 & EK_COLLATE == 0 && !is_vector(&l)
        {
            // One constant item (no COLLATE): the pin rewrites it
            // to `x = +item`.
            self.set_h::<E>(lh.max(first.0 + 1) + 1, lk & first.1 & EK_CONST)?;
        } else {
            self.set_h::<E>(lh.max(ih) + 1, lk & ik & EK_CONST)?;
        }
        self.negated::<E>(neg)?;
        Ok(Expr::InList(Box::new(l), list, neg))
    }

    /// A prefix position: `NOT`, the unary operators, or a
    /// primary.
    fn expr_prefix<E>(&mut self) -> Result<Expr, E> {
        if self.eat_kw(b"NOT") {
            // `NOT expr`.
            let e = self.operand::<E>(1, BP_EQ)?;
            self.node::<E>(0, EK_CONST)?;
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
            clause: None,
        };
        Ok(if neg { Expr::Not(Box::new(call)) } else { call })
    }

    /// Apply the pin's unary `+` / `-` rule to the expression just
    /// parsed: over a `+` node, `+` keeps it and `-` turns it into
    /// a `-` node (no new node either way); otherwise a new node,
    /// a `+` one for `plus`.
    fn sign_node<E>(&mut self, plus: bool) -> Result<(), E> {
        if self.ek & EK_UPLUS != 0 {
            if !plus {
                self.ek &= !EK_UPLUS;
            }
            return Ok(());
        }
        let k = self.ek & (EK_CONST | EK_LIT);
        self.set_h::<E>(
            self.eh.saturating_add(1),
            k | if plus { EK_UPLUS } else { 0 },
        )
    }

    fn expr_unary<E>(&mut self) -> Result<Expr, E> {
        // Signed literals fold at this level (literal_at handles
        // the sign itself).
        if let Some(e) = self.signed_literal_expr::<E>()? {
            return Ok(e);
        }
        match self.cur().tok {
            Tok::Minus => self.expr_neg::<E>(),
            Tok::Plus => {
                self.pos += 1;
                let e = self.unary_operand::<E>()?;
                self.sign_node::<E>(true)?;
                Ok(Expr::Pos(Box::new(e)))
            }
            #[cfg(feature = "v16")]
            Tok::BitNot => {
                self.pos += 1;
                let e = self.unary_operand::<E>()?;
                self.node::<E>(0, EK_CONST)?;
                Ok(Expr::BitNot(Box::new(e)))
            }
            _ => self.expr_primary::<E>(),
        }
    }

    /// A literal under any run of unary signs, consumed: one stack
    /// symbol per sign and the literal's own, and the pin's height
    /// for the sign nodes. `None` when no literal starts here.
    #[inline(never)]
    fn signed_literal_expr<E>(&mut self) -> Result<Option<Expr>, E> {
        let Some((lit, used)) = self.literal_at(self.pos) else {
            return Ok(None);
        };
        self.need::<E>(used)?;
        let last = self.pos + used - 1;
        let leaf = match lit {
            Literal::Null => EK_CONST,
            Literal::Integer(_) if self.literal_word(last) => EK_CONST,
            _ => EK_CONST | EK_LIT,
        };
        self.set_h::<E>(1, leaf)?;
        let mut i = last;
        while i > self.pos {
            i -= 1;
            let plus = matches!(self.toks.as_slice().get(i).map(|t| t.tok), Some(Tok::Plus));
            self.sign_node::<E>(plus)?;
        }
        self.pos += used;
        Ok(Some(Expr::Literal(lit)))
    }

    /// Unary `-` over a non-literal operand.
    #[inline(never)]
    fn expr_neg<E>(&mut self) -> Result<Expr, E> {
        self.pos += 1;
        let start = self.pos;
        let e = self.unary_operand::<E>()?;
        self.sign_node::<E>(false)?;
        if let Some(x) = self.neg_special(start) {
            return Ok(x);
        }
        Ok(Expr::Neg(Box::new(e)))
    }

    /// `-(9223372036854775808)`: parentheses (and unary `+`) are
    /// invisible to the pin's integer rule — and to its "hex
    /// literal too big" for `-` over `0x8000000000000000` (v0.16).
    /// The operand spans tokens `start..pos`.
    #[inline(never)]
    fn neg_special(&self, start: usize) -> Option<Expr> {
        let toks = self.toks.as_slice().get(start..self.pos).unwrap_or(&[]);
        let only = |f: &dyn Fn(usize) -> bool| {
            toks.iter().enumerate().all(|(k, t)| {
                matches!(t.tok, Tok::LParen | Tok::RParen | Tok::Plus) || f(start + k)
            })
        };
        if only(&|k| self.min_magnitude(k)) && toks.iter().any(|t| matches!(t.tok, Tok::Real(_))) {
            return Some(Expr::Literal(Literal::Integer(i64::MIN)));
        }
        if only(&|k| self.hex_min(k)) && toks.iter().any(|t| matches!(t.tok, Tok::Integer(_))) {
            return Some(Expr::HexTooBig(self.cur().offset));
        }
        None
    }

    /// The operand of a unary `-` / `+` / `~`: one symbol (the
    /// operator) below it.
    fn unary_operand<E>(&mut self) -> Result<Expr, E> {
        let h0 = self.held;
        self.held = h0.saturating_add(1);
        self.peak::<E>(self.held)?;
        let r = self.expr_prefix::<E>();
        self.held = h0;
        r
    }

    /// Token `i` is the keyword TRUE or FALSE (a literal the pin
    /// does not fold under ISNULL).
    fn literal_word(&self, i: usize) -> bool {
        matches!(
            self.toks.as_slice().get(i).map(|t| t.tok),
            Some(Tok::Ident(..))
        )
    }

    /// A primary: a literal, a column, a parameter, a function
    /// call, CASE, CAST, a parenthesised expression, a row value or
    /// a subquery. The nesting forms live in functions of their
    /// own, so that the frame each nesting level puts on the stack
    /// stays small.
    fn expr_primary<E>(&mut self) -> Result<Expr, E> {
        match self.cur().tok {
            #[cfg(feature = "v04")]
            Tok::HexTooBig => {
                let off = self.cur().offset;
                self.pos += 1;
                self.need::<E>(1)?;
                self.set_h::<E>(1, EK_CONST | EK_LIT)?;
                Ok(Expr::HexTooBig(off))
            }
            Tok::LParen => self.expr_paren::<E>(),
            #[cfg(feature = "params")]
            Tok::Param(i, h) => {
                self.pos += 1;
                self.need::<E>(1)?;
                self.set_h::<E>(1, EK_CONST)?;
                Ok(Expr::Param(i, h))
            }
            Tok::Ident(h, quoted) => {
                #[cfg(feature = "v06")]
                if self.cur_is_kw(b"EXISTS") {
                    return self.expr_exists::<E>();
                }
                if self.cur_is_kw(b"CASE") {
                    return self.expr_case::<E>();
                }
                // Function call: ident followed by '('.
                if !quoted
                    && matches!(
                        self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
                        Some(Tok::LParen)
                    )
                {
                    if eq_ci(handle_bytes(self.arena, h), b"CAST") {
                        return self.expr_cast::<E>();
                    }
                    #[cfg(feature = "triggers")]
                    if eq_ci(handle_bytes(self.arena, h), b"RAISE") {
                        return self.expr_raise::<E>();
                    }
                    return self.expr_call::<E>(h);
                }
                self.expr_column::<E>(h)
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        }
    }

    /// A column reference `name` or `table.name` (or a NULL /
    /// TRUE / FALSE literal) at the cursor.
    #[inline(never)]
    fn expr_column<E>(&mut self, h: ABytes) -> Result<Expr, E> {
        if let Some((lit, used)) = self.literal_at(self.pos) {
            self.pos += used;
            self.need::<E>(1)?;
            self.set_h::<E>(1, EK_CONST)?;
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
                    // `nm DOT nm`: the pin's DOT node over
                    // two leaves.
                    self.need::<E>(3)?;
                    self.set_h::<E>(2, 0)?;
                    return Ok(Expr::Column {
                        table: Some(h),
                        name: h2,
                    });
                }
                _ => return Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
            }
        }
        self.need::<E>(1)?;
        let k = if is_ctime_kw(handle_bytes(self.arena, h)) {
            EK_CONST
        } else {
            0
        };
        self.set_h::<E>(1, k)?;
        Ok(Expr::Column {
            table: None,
            name: h,
        })
    }

    /// `EXISTS (select)` (v0.6).
    #[cfg(feature = "v06")]
    #[inline(never)]
    fn expr_exists<E>(&mut self) -> Result<Expr, E> {
        self.pos += 1;
        self.expect::<E>(Tok::LParen)?;
        let mut extra_parens = 0usize;
        while matches!(self.cur().tok, Tok::LParen) {
            self.pos += 1;
            extra_parens += 1;
        }
        if !self.cur_is_kw(b"SELECT")
            && !(cfg!(feature = "v09") && self.cur_is_kw(b"VALUES"))
            && !self.with_at(self.pos)
        {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        }
        // `EXISTS LP select RP`.
        let stmt = self.subquery::<E>(2 + extra_parens)?;
        for _ in 0..=extra_parens {
            self.expect::<E>(Tok::RParen)?;
        }
        self.need::<E>(4)?;
        self.set_h::<E>(self.sel_h + 1, 0)?;
        Ok(Expr::ExistsSubquery(Box::new(stmt)))
    }

    /// `( ... )`: a scalar subquery, a parenthesised expression or
    /// (`v16`) a row value.
    #[inline(never)]
    fn expr_paren<E>(&mut self) -> Result<Expr, E> {
        self.pos += 1;
        if self.cur_is_kw(b"SELECT")
            || (cfg!(feature = "v09") && self.cur_is_kw(b"VALUES"))
            || self.with_at(self.pos)
        {
            return self.paren_subquery::<E>();
        }
        // `LP expr RP` (no node of its own).
        let e = self.operand_expr::<E>(1)?;
        #[cfg(feature = "v16")]
        if matches!(self.cur().tok, Tok::Comma) {
            return self.paren_vector::<E>(e);
        }
        self.expect::<E>(Tok::RParen)?;
        self.need::<E>(3)?;
        Ok(e)
    }

    /// `LP select RP`: a scalar subquery.
    #[inline(never)]
    fn paren_subquery<E>(&mut self) -> Result<Expr, E> {
        #[cfg(feature = "v06")]
        {
            let stmt = self.subquery::<E>(1)?;
            self.expect::<E>(Tok::RParen)?;
            self.need::<E>(3)?;
            self.set_h::<E>(self.sel_h + 1, 0)?;
            Ok(Expr::ScalarSubquery(Box::new(stmt)))
        }
        #[cfg(not(feature = "v06"))]
        {
            Err(perr(self.cur().offset, ParseKind::UnsupportedClause))
        }
    }

    /// The rest of a row value `(a, b, ...)` after its first
    /// element (v0.16, `v16`): `LP nexprlist COMMA expr RP`, a node
    /// of height 1 on the pin whatever its elements.
    #[cfg(feature = "v16")]
    #[inline(never)]
    fn paren_vector<E>(&mut self, first: Expr) -> Result<Expr, E> {
        let mut k = self.ek;
        let mut items = AVec::new();
        items.push(first);
        while self.eat(Tok::Comma) {
            items.push(self.operand_expr::<E>(3)?);
            k &= self.ek;
        }
        self.expect::<E>(Tok::RParen)?;
        self.need::<E>(5)?;
        self.set_h::<E>(1, k & EK_CONST)?;
        Ok(Expr::Vector(items))
    }

    /// `CASE [base] WHEN ... THEN ... [ELSE ...] END`.
    #[inline(never)]
    fn expr_case<E>(&mut self) -> Result<Expr, E> {
        self.pos += 1;
        // `CASE case_operand case_exprlist case_else END`: the
        // operand one symbol up; the first WHEN three (`CASE
        // case_operand WHEN`), later ones four; THEN two more.
        let (mut h, mut k) = (0usize, EK_CONST);
        let base = if self.cur_is_kw(b"WHEN") {
            None
        } else {
            let b = self.operand_expr::<E>(1)?;
            (h, k) = (self.eh, self.ek);
            Some(Box::new(b))
        };
        let mut whens = AVec::new();
        while self.eat_kw(b"WHEN") {
            let at = if whens.is_empty() { 3 } else { 4 };
            let w = self.operand_expr::<E>(at)?;
            (h, k) = (h.max(self.eh), k & self.ek);
            self.expect_kw::<E>(b"THEN")?;
            let t = self.operand_expr::<E>(at + 2)?;
            (h, k) = (h.max(self.eh), k & self.ek);
            whens.push((w, t));
        }
        if whens.is_empty() {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        }
        let otherwise = if self.eat_kw(b"ELSE") {
            let o = self.operand_expr::<E>(4)?;
            (h, k) = (h.max(self.eh), k & self.ek);
            Some(Box::new(o))
        } else {
            None
        };
        self.expect_kw::<E>(b"END")?;
        self.need::<E>(5)?;
        self.set_h::<E>(h + 1, k & EK_CONST)?;
        Ok(Expr::Case {
            base,
            whens,
            otherwise,
        })
    }

    /// `CAST(expr AS type)`.
    #[inline(never)]
    fn expr_cast<E>(&mut self) -> Result<Expr, E> {
        self.pos += 2; // name + '('
        // `CAST LP expr AS typetoken RP`.
        let e = self.operand_expr::<E>(2)?;
        let (h, k) = (self.eh, self.ek);
        self.expect_kw::<E>(b"AS")?;
        // Type name: raw SQL slice up to ')'.
        let start = self.cur().offset;
        let mut depth = 0usize;
        let mut args = 0usize;
        while !self.at_eof() {
            match self.cur().tok {
                Tok::LParen => {
                    depth += 1;
                    args = args.max(1);
                }
                Tok::Comma if depth == 1 => args = 2,
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
        let ty = self.arena.alloc_copy_handle(trim(self.span(start, end)))?;
        self.expect::<E>(Tok::RParen)?;
        // `typename [LP signed [COMMA signed] RP]` peaks above
        // `CAST LP expr AS`.
        self.need::<E>(6 + 2 * args)?;
        self.eh = h;
        self.ek = k;
        self.node::<E>(0, EK_CONST)?;
        Ok(Expr::Cast(Box::new(e), ty))
    }

    /// A function call `name(...)`: `*`, `DISTINCT`, the
    /// arguments, the aggregate ORDER BY and FILTER (`v16`).
    #[inline(never)]
    fn expr_call<E>(&mut self, fname: ABytes) -> Result<Expr, E> {
        self.pos += 2; // name + '('
        let star = self.eat(Tok::Star);
        let distinct = !star && self.eat_kw(b"DISTINCT");
        let mut args = AVec::new();
        #[cfg(feature = "v16")]
        let no_args = self.cur_is_kw(b"ORDER");
        #[cfg(not(feature = "v16"))]
        let no_args = false;
        // `idj LP distinct [nexprlist COMMA] expr`: the first
        // argument three symbols up, later ones five.
        let (mut h, mut k) = (0usize, EK_CONST);
        if !star && !no_args && !matches!(self.cur().tok, Tok::RParen) {
            loop {
                let at = if args.is_empty() { 3 } else { 5 };
                args.push(self.operand_expr::<E>(at)?);
                (h, k) = (h.max(self.eh), k & self.ek);
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        if args.len() > MAX_FUNCTION_ARG {
            return Err(perr(self.cur().offset, ParseKind::TooManyArguments));
        }
        // `f(args ORDER BY ...)` (v0.16, `v16`): the pin's
        // grammar takes it after a (possibly empty) argument
        // list, not after `*` (`count(* ORDER BY x)` is a
        // syntax error there). With no arguments the pin's
        // `sqlite3ExprAddFunctionOrderBy` drops the clause
        // unresolved: `count(ORDER BY nosuch)` is `count(*)`
        // (review fix: it was refused). Its terms (`idj LP
        // distinct exprlist ORDER BY sortlist`) add nothing to
        // the call's height.
        #[cfg(feature = "v16")]
        let clause = self.call_clauses::<E>(star, args.is_empty())?;
        #[cfg(not(feature = "v16"))]
        {
            self.expect::<E>(Tok::RParen)?;
            self.need::<E>(if star { 4 } else { 5 })?;
        }
        self.set_h::<E>(h + 1, k & EK_CONST)?;
        Ok(Expr::Func {
            name: fname,
            distinct,
            star,
            args,
            #[cfg(feature = "v16")]
            clause,
        })
    }

    /// A call's tail after its arguments (v0.16, `v16`): the
    /// aggregate ORDER BY, the `)` and FILTER. Neither clause adds
    /// to the call's height.
    #[cfg(feature = "v16")]
    #[inline(never)]
    fn call_clauses<E>(
        &mut self,
        star: bool,
        no_args: bool,
    ) -> Result<Option<Box<crate::sql::ast::FuncClause>>, E> {
        // `idj LP distinct exprlist ORDER BY sortlist`.
        let order_by = if !star && self.eat_kw(b"ORDER") {
            self.expect_kw::<E>(b"BY")?;
            let terms = self.order_terms::<E>(self.held + 6, false)?;
            if no_args { AVec::new() } else { terms }
        } else {
            AVec::new()
        };
        self.expect::<E>(Tok::RParen)?;
        // `idj LP distinct exprlist RP` (`idj LP STAR RP`).
        self.need::<E>(if star { 4 } else { 5 })?;
        // `FILTER (WHERE expr)`: FILTER is no reserved word, so
        // only a FILTER followed by `(` starts the clause and
        // `count(*) filter` stays an alias (`... RP FILTER LP
        // WHERE expr`).
        let filter = if self.cur_is_kw(b"FILTER")
            && matches!(
                self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
                Some(Tok::LParen)
            ) {
            self.pos += 2;
            self.expect_kw::<E>(b"WHERE")?;
            let f = self.operand_expr::<E>(if star { 7 } else { 8 })?;
            self.expect::<E>(Tok::RParen)?;
            Some(f)
        } else {
            None
        };
        // `OVER (...)` / `OVER name` (v0.16, `window`): after the
        // FILTER, as in the pin's `filter_over` (`OVER () FILTER`
        // is a syntax error there).
        #[cfg(feature = "window")]
        let over = self.over_clause::<E>()?;
        #[cfg(feature = "window")]
        let none = over.is_none();
        #[cfg(not(feature = "window"))]
        let none = true;
        Ok(if order_by.is_empty() && filter.is_none() && none {
            None
        } else {
            Some(Box::new(crate::sql::ast::FuncClause {
                order_by,
                filter,
                #[cfg(feature = "window")]
                over,
            }))
        })
    }

    /// A call's `OVER` clause (v0.16, `window`). OVER is a keyword
    /// only right after the call's `)` with `(` or a name after it
    /// (the pin's `analyzeOverKeyword`), so `sum(x) over` is still
    /// an alias. A "name" there is the pin's `getToken` ID: a
    /// string, a quoted or fallback identifier, a join keyword —
    /// never a keyword like FROM, WHERE, UNION or LIMIT (v0.16
    /// review fix: `count(*) over FROM t` is an alias).
    #[cfg(feature = "window")]
    #[inline(never)]
    fn over_clause<E>(&mut self) -> Result<Option<crate::sql::ast::Over>, E> {
        use crate::sql::ast::Over;
        if !self.cur_is_kw(b"OVER") {
            return Ok(None);
        }
        let i = self.pos + 1;
        match self.toks.as_slice().get(i).map(|t| t.tok) {
            Some(Tok::Ident(_, false)) if self.reserved(i, true) || self.is_kw(i, b"INDEXED") => {
                Ok(None)
            }
            Some(Tok::LParen) => {
                self.pos += 2;
                // `... RP OVER LP`.
                let w = self.window_def::<E>(self.held + 8)?;
                self.expect::<E>(Tok::RParen)?;
                Ok(Some(Over::Spec(Box::new(w))))
            }
            Some(Tok::Ident(..)) | Some(Tok::Text(_)) => {
                let h = self.window_name::<E>(self.pos + 1)?;
                self.pos += 2;
                Ok(Some(Over::Named(h)))
            }
            _ => Ok(None),
        }
    }

    /// A window name token `i` as the pin keeps one: its raw text
    /// with any quotes (`OVER [w]` names no window `w`), compared
    /// case-insensitively (v0.16, `window`).
    #[cfg(feature = "window")]
    fn window_name<E>(&self, i: usize) -> Result<ABytes, E> {
        let t = self.toks.as_slice().get(i.min(self.limit - 1)).copied();
        let Some(t) = t else {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        };
        let quoted = match t.tok {
            Tok::Ident(h, false) => return Ok(h),
            Tok::Ident(_, true) | Tok::Text(_) => true,
            _ => false,
        };
        if !quoted {
            return Err(perr(t.offset, ParseKind::UnexpectedToken));
        }
        let b = self.sql;
        let q = b.get(t.offset).copied().unwrap_or(b'"');
        let close = if q == b'[' { b']' } else { q };
        let mut j = t.offset + 1;
        while j < b.len() {
            if b.get(j) == Some(&close) {
                if close != b']' && b.get(j + 1) == Some(&close) {
                    j += 2;
                    continue;
                }
                j += 1;
                break;
            }
            j += 1;
        }
        Ok(self.arena.alloc_copy_handle(self.span(t.offset, j))?)
    }

    /// A window definition inside its parentheses (v0.16,
    /// `window`; the pin's `window` rule): `[base] [PARTITION BY
    /// expr, ...] [ORDER BY term, ...] [frame]`. `held`
    /// approximates the pin's stack below its expressions.
    #[cfg(feature = "window")]
    #[inline(never)]
    fn window_def<E>(&mut self, held: usize) -> Result<crate::sql::ast::WindowDef, E> {
        const KWS: &[&[u8]] = &[b"PARTITION", b"ORDER", b"RANGE", b"ROWS", b"GROUPS"];
        let base = match self.cur().tok {
            Tok::Ident(_, q) if q || !KWS.iter().any(|k| self.cur_is_kw(k)) => {
                let h = self.window_name::<E>(self.pos)?;
                self.pos += 1;
                Some(h)
            }
            Tok::Text(_) => {
                let h = self.window_name::<E>(self.pos)?;
                self.pos += 1;
                Some(h)
            }
            _ => None,
        };
        let mut partition = AVec::new();
        if self.eat_kw(b"PARTITION") {
            self.expect_kw::<E>(b"BY")?;
            loop {
                let at = if partition.is_empty() { held } else { held + 2 };
                let mut e = self.window_expr::<E>(at)?;
                mark_ctx(&mut e, self.eh);
                partition.push(e);
                if !self.eat(Tok::Comma) {
                    break;
                }
            }
            if partition.len() > MAX_COLUMN {
                return Err(perr(self.cur().offset, ParseKind::TooManyTerms));
            }
        }
        let order_by = if self.eat_kw(b"ORDER") {
            self.expect_kw::<E>(b"BY")?;
            self.order_terms::<E>(held + 3, false)?
        } else {
            AVec::new()
        };
        let frame = self.frame_opt::<E>(held + 4)?;
        Ok(crate::sql::ast::WindowDef {
            base,
            partition,
            order_by,
            frame,
        })
    }

    /// One expression of a window definition, `held` symbols up.
    #[cfg(feature = "window")]
    fn window_expr<E>(&mut self, held: usize) -> Result<Expr, E> {
        let h0 = self.held;
        self.held = held;
        self.peak::<E>(self.held)?;
        let r = self.parse_expr::<E>();
        self.held = h0;
        r
    }

    /// A window's optional frame (v0.16, `window`; the pin's
    /// `frame_opt`): `ROWS | RANGE | GROUPS` then one start bound
    /// (the end is CURRENT ROW) or `BETWEEN start AND end`, then
    /// an optional `EXCLUDE`. A start that comes after the end in
    /// the pin's bound order is its "unsupported frame
    /// specification".
    #[cfg(feature = "window")]
    #[inline(never)]
    fn frame_opt<E>(&mut self, held: usize) -> Result<Option<crate::sql::ast::Frame>, E> {
        use crate::sql::ast::{Frame, FrameBound, FrameExclude, FrameUnit};
        let unit = if self.eat_kw(b"ROWS") {
            FrameUnit::Rows
        } else if self.eat_kw(b"RANGE") {
            FrameUnit::Range
        } else if self.eat_kw(b"GROUPS") {
            FrameUnit::Groups
        } else {
            return Ok(None);
        };
        let at = self.cur().offset;
        let (start, end) = if self.eat_kw(b"BETWEEN") {
            let s = self.frame_bound::<E>(held + 2, true)?;
            self.expect_kw::<E>(b"AND")?;
            let e = self.frame_bound::<E>(held + 4, false)?;
            (s, e)
        } else {
            (
                self.frame_bound::<E>(held + 1, true)?,
                FrameBound::CurrentRow,
            )
        };
        if matches!(
            (&start, &end),
            (FrameBound::CurrentRow, FrameBound::Preceding(_))
                | (
                    FrameBound::Following(_),
                    FrameBound::Preceding(_) | FrameBound::CurrentRow
                )
        ) {
            return Err(perr(at, ParseKind::UnsupportedFrame));
        }
        let exclude = if self.eat_kw(b"EXCLUDE") {
            if self.eat_kw(b"NO") {
                self.expect_kw::<E>(b"OTHERS")?;
                FrameExclude::NoOthers
            } else if self.eat_kw(b"CURRENT") {
                self.expect_kw::<E>(b"ROW")?;
                FrameExclude::CurrentRow
            } else if self.eat_kw(b"GROUP") {
                FrameExclude::Group
            } else {
                self.expect_kw::<E>(b"TIES")?;
                FrameExclude::Ties
            }
        } else {
            FrameExclude::None
        };
        Ok(Some(Frame {
            unit,
            start,
            end,
            exclude,
        }))
    }

    /// One frame boundary (v0.16, `window`): `UNBOUNDED PRECEDING`
    /// (a start only), `UNBOUNDED FOLLOWING` (an end only),
    /// `CURRENT ROW`, or `expr PRECEDING | FOLLOWING` — an offset
    /// that is not a constant expression becomes NULL, as the
    /// pin's `sqlite3WindowOffsetExpr` makes it.
    #[cfg(feature = "window")]
    fn frame_bound<E>(
        &mut self,
        held: usize,
        start: bool,
    ) -> Result<crate::sql::ast::FrameBound, E> {
        use crate::sql::ast::FrameBound;
        if self.eat_kw(b"UNBOUNDED") {
            return Ok(if start {
                self.expect_kw::<E>(b"PRECEDING")?;
                FrameBound::UnboundedPreceding
            } else {
                self.expect_kw::<E>(b"FOLLOWING")?;
                FrameBound::UnboundedFollowing
            });
        }
        if self.eat_kw(b"CURRENT") {
            self.expect_kw::<E>(b"ROW")?;
            return Ok(FrameBound::CurrentRow);
        }
        let e = self.window_expr::<E>(held)?;
        let e = if offset_is_constant(&e) {
            e
        } else {
            Expr::Literal(Literal::Null)
        };
        if self.eat_kw(b"PRECEDING") {
            Ok(FrameBound::Preceding(Box::new(e)))
        } else {
            self.expect_kw::<E>(b"FOLLOWING")?;
            Ok(FrameBound::Following(Box::new(e)))
        }
    }

    /// A core's `WINDOW name AS (...), ...` clause (v0.16,
    /// `window`), after HAVING. Each definition after the first
    /// takes its base window's PARTITION BY / ORDER BY from the
    /// definitions before it (the pin's `sqlite3WindowChain`;
    /// the first definition's base is never looked up, as on the
    /// pin).
    #[cfg(feature = "window")]
    #[inline(never)]
    fn window_clause<E>(
        &mut self,
        held: usize,
    ) -> Result<AVec<(ABytes, crate::sql::ast::WindowDef)>, E> {
        let mut out: AVec<(ABytes, crate::sql::ast::WindowDef)> = AVec::new();
        if !self.window_clause_at(self.pos) {
            return Ok(out);
        }
        self.pos += 1;
        loop {
            let name = self.window_name::<E>(self.pos)?;
            self.pos += 1;
            self.expect_kw::<E>(b"AS")?;
            self.expect::<E>(Tok::LParen)?;
            let mut w = self.window_def::<E>(held + 4)?;
            self.expect::<E>(Tok::RParen)?;
            if !out.is_empty() {
                crate::sql::ast::chain_window(&mut w, out.as_slice(), self.arena)
                    .map_err(|kind| Error::Schema { kind })?;
            }
            out.push((name, w));
            if !self.eat(Tok::Comma) {
                break;
            }
        }
        Ok(out)
    }

    // ---------- rich SELECT ----------

    /// `select_rich` with `held` symbols below it on the pin's
    /// parser stack.
    fn select_at<E>(&mut self, held: usize) -> Result<SelectStmt, E> {
        let h0 = self.held;
        self.held = held;
        self.peak::<E>(self.held)?;
        let r = self.select_rich::<E>();
        self.held = h0;
        r
    }

    /// A full select statement (v0.9 shape): one core, an
    /// optional compound tail, and the trailing ORDER BY /
    /// LIMIT that belong to the whole statement. Without the
    /// `v09` feature this is exactly the pre-v0.9 single core
    /// plus its tail.
    ///
    /// `held` is the pin's parser stack below it; afterwards
    /// `sel_h` / `sel_r` are its height and resolution depth (the
    /// enclosing select's accumulators are kept).
    fn select_rich<E>(&mut self) -> Result<SelectStmt, E> {
        let saved = (self.acc_h, self.acc_r, self.rsub, self.on);
        self.acc_h = 0;
        self.acc_r = 0;
        #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
        let mut r = self.select_body::<E>();
        (self.sel_h, self.sel_r) = (self.acc_h, self.acc_r);
        (self.acc_h, self.acc_r, self.rsub, self.on) = saved;
        #[cfg(feature = "v16")]
        if let Ok(s) = &mut r {
            s.rdepth = u32::try_from(self.sel_r).unwrap_or(u32::MAX);
        }
        r
    }

    fn select_body<E>(&mut self) -> Result<SelectStmt, E> {
        let hs = self.held;
        // `select ::= WITH [RECURSIVE] wqlist selectnowith`: the
        // cores sit two symbols up (three after RECURSIVE).
        #[cfg(feature = "v16")]
        let (with, base) = if self.with_at(self.pos) {
            let recursive = self.is_kw(self.pos + 1, b"RECURSIVE");
            let w = self.with_clause::<E>(hs)?;
            (Some(Box::new(w)), hs + 2 + usize::from(recursive))
        } else {
            (None, hs)
        };
        #[cfg(not(feature = "v16"))]
        let base = hs;
        let mut head = self.select_core::<E>(base)?;
        #[cfg(feature = "v16")]
        {
            head.with = with;
        }
        // The pin gives every result column of a compound member a
        // node more (a COLLATE it adds for the sorter) when the
        // member is an operand of UNION / EXCEPT / INTERSECT, or the
        // compound has an ORDER BY (UNION ALL alone adds none):
        // `pending` is the tallest result column of the members not
        // known yet to be such operands.
        #[cfg(feature = "v09")]
        let mut pending = self.core_h;
        // The base of the last core: `selectnowith
        // multiselect_op oneselect` puts a later one two up.
        #[cfg_attr(not(feature = "v09"), allow(unused_mut))]
        let mut last = base;
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
                last = base + 2;
                let core = self.select_core::<E>(last)?;
                if op == CompoundOp::UnionAll {
                    pending = pending.max(self.core_h);
                } else {
                    self.sorted_members::<E>(pending.max(self.core_h))?;
                    pending = 0;
                }
                compound.push((op, core));
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
        self.select_tail::<E>(&mut head, last)?;
        #[cfg(feature = "v09")]
        if !head.compound.is_empty() && !head.order_by.is_empty() {
            self.sorted_members::<E>(pending)?;
        }
        // The pin's `parserDoubleLinkSelect`: "too many terms in
        // compound SELECT" past 500 SELECTs (multi-row VALUES is
        // one), unless the last member is a one-row VALUES (its
        // `SF_Values` exemption; v0.16 review fix — probed: 600
        // SELECTs then `VALUES(1)` read, then `VALUES(1),(2)` do
        // not).
        #[cfg(feature = "v09")]
        if head.compound.len() >= MAX_COMPOUND_SELECT
            && head
                .compound
                .as_slice()
                .last()
                .is_none_or(|(_, c)| c.values.len() != 1)
        {
            return Err(perr(self.cur().offset, ParseKind::TooManyCompoundTerms));
        }
        Ok(head)
    }

    /// Compound members whose result columns reach height `h` go
    /// through the pin's sorter, which puts a node over each.
    #[cfg(feature = "v09")]
    fn sorted_members<E>(&self, h: usize) -> Result<(), E> {
        if h.saturating_add(1) > self.lim.expr_depth && h > 0 {
            return Err(perr(self.cur().offset, ParseKind::ExprTooDeep));
        }
        Ok(())
    }

    /// One compound operand: a SELECT core or — under `v09` —
    /// a VALUES core, with no ORDER BY / LIMIT / compound of
    /// its own (the pin gives cores none: `SELECT ... ORDER BY
    /// ... UNION ...` fails there with "ORDER BY clause should
    /// come after UNION"). `base` symbols sit below it on the
    /// pin's stack.
    fn select_core<E>(&mut self, base: usize) -> Result<SelectStmt, E> {
        #[cfg(feature = "v09")]
        if self.cur_is_kw(b"VALUES") {
            return self.values_core::<E>(base);
        }
        self.select_core_select::<E>(base)
    }

    /// A `VALUES (row), (row), ...` core (v0.9): values are
    /// full expressions, as in the INSERT grammar.
    #[cfg(feature = "v09")]
    fn values_core<E>(&mut self, base: usize) -> Result<SelectStmt, E> {
        let values = self.values_rows::<E>(base, false)?;
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
            #[cfg(feature = "v16")]
            with: None,
            #[cfg(feature = "v16")]
            rctx: 0,
            #[cfg(feature = "v16")]
            rdepth: 0,
            #[cfg(feature = "window")]
            windows: AVec::new(),
        })
    }

    fn select_core_select<E>(&mut self, base: usize) -> Result<SelectStmt, E> {
        self.expect_kw::<E>(b"SELECT")?;
        self.on = OnChain::default();
        let distinct = if self.eat_kw(b"DISTINCT") {
            true
        } else {
            let _ = self.eat_kw(b"ALL");
            false
        };
        let mut items = AVec::new();
        let mut core_h = 1usize;
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
                // `SELECT distinct sclp scanpt expr`.
                let e = self.item::<E>(base + 4)?;
                core_h = core_h.max(self.eh);
                let end = self.cur().offset;
                let text = self.arena.alloc_copy_handle(trim(self.span(start, end)))?;
                let alias = self.opt_alias();
                items.push(SelectItem::Expr(e, alias, text));
            }
            if self.eat(Tok::Comma) {
                continue;
            }
            break;
        }
        // "too many columns in result set": checked when the pin
        // resolves the select, so not in a CREATE VIEW or CTE body
        // (`defer`; the expansion checks a body it copies).
        if items.len() > MAX_COLUMN && !self.defer {
            return Err(perr(self.cur().offset, ParseKind::TooManyColumns));
        }
        // `SELECT distinct selcollist FROM`.
        #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
        let mut from = if self.eat_kw(b"FROM") {
            self.source_list::<E>(base + 4)?
        } else {
            AVec::new()
        };
        if from.len() > MAX_JOIN_TABLES {
            return Err(Error::Schema {
                kind: crate::error::SchemaKind::TooManyTables,
            });
        }
        // `SELECT distinct selcollist from WHERE expr`: the pin
        // resolves the WHERE with the ON / USING terms ANDed in.
        let (filter, wh) = if self.eat_kw(b"WHERE") {
            #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
            let mut w = self.raw::<E>(base + 5)?;
            self.fold_item::<E>(false)?;
            let wh = self.where_depth::<E>(Some(self.eh))?;
            #[cfg(feature = "v16")]
            mark_ctx(&mut w, wh);
            (Some(w), wh)
        } else {
            (None, self.where_depth::<E>(None)?)
        };
        // The ON / USING terms are resolved inside that WHERE tree.
        #[cfg(feature = "v16")]
        for f in from.as_mut_slice() {
            if let Some(on) = &mut f.on {
                mark_ctx(on, wh);
            }
        }
        #[cfg(not(feature = "v16"))]
        let _ = wh;
        let mut group_by = AVec::new();
        if self.eat_kw(b"GROUP") {
            self.expect_kw::<E>(b"BY")?;
            loop {
                // `... where_opt GROUP BY [nexprlist COMMA] expr`.
                let at = if group_by.is_empty() { 7 } else { 9 };
                group_by.push(self.sort_item::<E>(base + at)?);
                if self.eat(Tok::Comma) {
                    continue;
                }
                break;
            }
            if group_by.len() > MAX_COLUMN && !self.defer {
                return Err(perr(self.cur().offset, ParseKind::TooManyTerms));
            }
        }
        // `... where_opt groupby_opt HAVING expr`.
        let having = if self.eat_kw(b"HAVING") {
            Some(self.item::<E>(base + 7)?)
        } else {
            None
        };
        // `... having_opt WINDOW windowdefn_list` (v0.16, `window`).
        #[cfg(feature = "window")]
        let windows = self.window_clause::<E>(base + 8)?;
        // The core's end: `SELECT distinct selcollist from
        // where_opt groupby_opt having_opt orderby_opt limit_opt`.
        self.peak::<E>(base + 9)?;
        self.core_h = core_h;
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
            #[cfg(feature = "v16")]
            with: None,
            #[cfg(feature = "v16")]
            rctx: 0,
            #[cfg(feature = "v16")]
            rdepth: 0,
            #[cfg(feature = "window")]
            windows,
        })
    }

    /// Fold the WHERE clause the pin resolves — the written one
    /// (of height `w`, its subqueries' depth in `rsub`) with the
    /// core's ON / USING terms ANDed in — into the select.
    /// Returns that tree's height (0 when there is none).
    fn where_depth<E>(&mut self, w: Option<usize>) -> Result<usize, E> {
        if w.is_none() && self.on.n == 0 {
            return Ok(0);
        }
        let rsub = if w.is_some() { self.rsub } else { 0 };
        // A lone leaf WHERE in a view body (see `reduced`).
        let w = w.map(|h| usize::from(!(self.reduced && h == 1 && self.on.n == 0)) * h);
        let h = self.on.height(w);
        let depth = h.saturating_add(rsub.max(self.on.rsub));
        if depth > self.lim.expr_depth && !self.defer {
            return Err(perr(self.cur().offset, ParseKind::ExprTooDeep));
        }
        self.acc_r = self.acc_r.max(depth);
        Ok(h)
    }

    /// A comma-separated ORDER BY term list after `ORDER BY`:
    /// the statement's (v0.4) and, under `v16`, an aggregate's
    /// own `f(args ORDER BY ...)` (the same grammar on the pin).
    /// `held` symbols sit below the first term on the pin's stack
    /// (`sortlist COMMA` two more below the others); a statement's
    /// terms are resolved on their own (`resolve`), an aggregate's
    /// are part of its call.
    fn order_terms<E>(&mut self, held: usize, resolve: bool) -> Result<AVec<(Expr, SortOrder)>, E> {
        let mut order_by = AVec::new();
        loop {
            let at = if order_by.is_empty() { held } else { held + 2 };
            let e = if resolve {
                self.sort_item::<E>(at)?
            } else {
                let h0 = self.held;
                self.held = at;
                self.peak::<E>(self.held)?;
                let r = self.parse_expr::<E>();
                self.held = h0;
                #[cfg_attr(not(feature = "v16"), allow(unused_mut))]
                let mut e = r?;
                // An aggregate's or window's term: inside a call, the
                // whole item's height is marked over this later.
                #[cfg(feature = "v16")]
                mark_ctx(&mut e, self.eh);
                e
            };
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
        if order_by.len() > MAX_COLUMN && !(resolve && self.defer) {
            return Err(perr(self.cur().offset, ParseKind::TooManyTerms));
        }
        Ok(order_by)
    }

    /// The trailing ORDER BY / LIMIT of a select statement,
    /// parsed into `head` after its core(s) (v0.9 split: cores
    /// carry none of their own). `base` is the last core's.
    #[inline(never)]
    fn select_tail<E>(&mut self, head: &mut SelectStmt, base: usize) -> Result<(), E> {
        let mut order_by = AVec::new();
        if self.eat_kw(b"ORDER") {
            self.expect_kw::<E>(b"BY")?;
            // `... having_opt ORDER BY sortlist`.
            order_by = self.order_terms::<E>(base + 9, true)?;
        }
        let mut limit = None;
        let mut offset = None;
        if self.eat_kw(b"LIMIT") {
            // `... orderby_opt LIMIT expr [OFFSET | COMMA expr]`:
            // one TK_LIMIT node over both on the pin.
            let first = self.limit_term::<E>(base + 9)?;
            let (mut h, mut rsub) = (self.eh, self.rsub);
            let comma = self.eat(Tok::Comma);
            let second = if comma || self.eat_kw(b"OFFSET") {
                let s = self.limit_term::<E>(base + 11)?;
                h = h.max(self.eh);
                rsub = rsub.max(self.rsub);
                Some(s)
            } else {
                None
            };
            self.set_h::<E>(h + 1, 0)?;
            self.rsub = rsub;
            self.fold_item::<E>(true)?;
            #[cfg(feature = "v16")]
            let (mut first, mut second) = (first, second);
            #[cfg(feature = "v16")]
            {
                mark_ctx(&mut first, h + 1);
                if let Some(x) = &mut second {
                    mark_ctx(x, h + 1);
                }
            }
            match second {
                // `LIMIT offset, count`.
                Some(s) if comma => {
                    offset = Some(first);
                    limit = Some(s);
                }
                Some(s) => {
                    limit = Some(first);
                    offset = Some(s);
                }
                None => limit = Some(first),
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
    /// pin's `MustBeInt` rule. `held` symbols sit below it.
    fn limit_term<E>(&mut self, held: usize) -> Result<Box<Expr>, E> {
        #[cfg(feature = "v16")]
        {
            Ok(Box::new(self.raw::<E>(held)?))
        }
        #[cfg(not(feature = "v16"))]
        match self.literal_at(self.pos) {
            Some((lit @ Literal::Integer(_), used)) => {
                // The LIMIT node sits above the run.
                self.sign_run::<E>(self.pos, used, held, 1)?;
                self.pos += used;
                self.eh = 1;
                self.rsub = 0;
                Ok(Box::new(Expr::Literal(lit)))
            }
            _ => Err(perr(self.cur().offset, ParseKind::UnsupportedClause)),
        }
    }

    /// `SELECT * FROM <table>` as an AST (v0.16: the subquery an
    /// `x IN table` operand stands for).
    #[cfg(feature = "v16")]
    fn table_select(&self, table: ABytes, qualified: bool) -> SelectStmt {
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
            args: None,
            qualified,
            cte: None,
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
            with: None,
            rctx: 0,
            rdepth: 0,
            #[cfg(feature = "window")]
            windows: AVec::new(),
        }
    }

    /// A FROM clause body (v0.4; shared by SELECT and UPDATE
    /// ... FROM): the first source, then `,` / join-operator
    /// separated sources, each joined to everything on its left.
    /// A FROM clause's sources, `fb` symbols (up to its `FROM`)
    /// below them on the pin's stack.
    fn source_list<E>(&mut self, fb: usize) -> Result<AVec<FromSource>, E> {
        let mut from = AVec::new();
        from.push(self.join_source::<E>(JoinKind::Inner, false, true, fb)?);
        loop {
            if self.eat(Tok::Comma) {
                from.push(self.join_source::<E>(JoinKind::Cross, false, false, fb)?);
                continue;
            }
            let Some((kind, natural)) = self.join_op::<E>()? else {
                break;
            };
            from.push(self.join_source::<E>(kind, natural, false, fb)?);
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
    ///
    /// `on_at` symbols sit below an ON expression on the pin's
    /// stack; ON and USING terms join the core's `OnChain`.
    fn join_constraint<E>(
        &mut self,
        natural: bool,
        first: bool,
        on_at: usize,
    ) -> Result<(Option<Expr>, AVec<ABytes>), E> {
        let mut using = AVec::new();
        if first {
            return Ok((None, using));
        }
        let on = if self.eat_kw(b"ON") {
            let e = self.raw::<E>(on_at)?;
            self.on.push(self.eh, self.rsub);
            Some(e)
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
                        // The pin's `a.x = b.x` term: an EQ node
                        // over two column leaves.
                        self.on.push(2, 0);
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

    /// One FROM source (`fb` as in `source_list`): a derived
    /// table or a named one, each parsed out of line (the frame of
    /// each nested FROM subquery level stays small).
    fn join_source<E>(
        &mut self,
        kind: JoinKind,
        natural: bool,
        first: bool,
        fb: usize,
    ) -> Result<FromSource, E> {
        #[cfg(feature = "v09")]
        if let Some(src) = self.derived_source::<E>(kind, natural, first, fb)? {
            return Ok(src);
        }
        self.table_source::<E>(kind, natural, first, fb)
    }

    /// A derived table source, or `None` (nothing consumed) when no
    /// subquery starts here.
    #[cfg(feature = "v09")]
    #[inline(never)]
    fn derived_source<E>(
        &mut self,
        kind: JoinKind,
        natural: bool,
        first: bool,
        fb: usize,
    ) -> Result<Option<FromSource>, E> {
        // A derived table (v0.9): `(SELECT ...)` or `(VALUES
        // ...)`, possibly under further parentheses (the pin
        // accepts `FROM ((SELECT 1)) s`). The alias is optional;
        // no column-name list may follow it (pin: syntax error).
        if matches!(self.cur().tok, Tok::LParen) {
            let mut depth = 0usize;
            let mut i = self.pos;
            while i < self.limit && matches!(self.toks.as_slice()[i].tok, Tok::LParen) {
                depth += 1;
                i += 1;
            }
            if self.is_kw(i, b"SELECT") || self.is_kw(i, b"VALUES") || self.with_at(i) {
                for _ in 0..depth {
                    self.expect::<E>(Tok::LParen)?;
                }
                // `stl_prefix LP` per parenthesis (the inner ones
                // the pin's `stl_prefix LP seltablist RP`); the
                // derived table's resolution does not sit in any
                // expression of this select.
                let h0 = self.held;
                self.held = fb.saturating_add(depth.saturating_mul(2));
                self.peak::<E>(self.held)?;
                let r = self.select_rich::<E>();
                self.held = h0;
                let inner = r?;
                self.acc_r = self.acc_r.max(self.sel_r);
                for _ in 0..depth {
                    self.expect::<E>(Tok::RParen)?;
                }
                let alias = self.opt_alias();
                // `stl_prefix LP select RP as ON expr`.
                let (on, using) = self.join_constraint::<E>(natural, first, fb + 6)?;
                return Ok(Some(FromSource {
                    table: ABytes::EMPTY,
                    alias,
                    kind,
                    on,
                    derived: Some(Box::new(inner)),
                    natural,
                    using,
                    #[cfg(feature = "v16")]
                    args: None,
                    #[cfg(feature = "v16")]
                    qualified: false,
                    #[cfg(feature = "v16")]
                    cte: None,
                }));
            }
        }
        Ok(None)
    }

    /// A named FROM source: a table, view or CTE name, or (`v16`) a
    /// table-valued function call.
    #[inline(never)]
    fn table_source<E>(
        &mut self,
        kind: JoinKind,
        natural: bool,
        first: bool,
        fb: usize,
    ) -> Result<FromSource, E> {
        #[cfg(feature = "v16")]
        let (table, qualified) = self.name_q::<E>()?;
        #[cfg(not(feature = "v16"))]
        let table = self.name::<E>()?;
        // A table-valued function (v0.16): `name(arg, ...)`.
        #[cfg(feature = "v16")]
        let args = if self.eat(Tok::LParen) {
            let mut v = AVec::new();
            if !self.eat(Tok::RParen) {
                loop {
                    // `stl_prefix nm dbnm LP [nexprlist COMMA] expr`.
                    let at = if v.is_empty() { 4 } else { 6 };
                    v.push(self.item::<E>(fb + at)?);
                    if !self.eat(Tok::Comma) {
                        break;
                    }
                }
                self.expect::<E>(Tok::RParen)?;
            }
            Some(v)
        } else {
            None
        };
        let alias = self.opt_alias();
        // `stl_prefix nm dbnm as ON expr`.
        let (on, using) = self.join_constraint::<E>(natural, first, fb + 5)?;
        Ok(FromSource {
            table,
            alias,
            kind,
            on,
            derived: None,
            natural,
            using,
            #[cfg(feature = "v16")]
            args,
            #[cfg(feature = "v16")]
            qualified,
            #[cfg(feature = "v16")]
            cte: None,
        })
    }

    // ---------- ALTER TABLE ADD COLUMN ----------

    fn alter_table<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"ALTER")?;
        self.expect_kw::<E>(b"TABLE")?;
        let table = self.name::<E>()?;
        // RENAME TO / RENAME [COLUMN] / DROP [COLUMN] (v0.16,
        // `v16`): every name is the pin's `nm` (an identifier, a
        // keyword that is not reserved, or a string).
        #[cfg(feature = "v16")]
        if self.eat_kw(b"RENAME") {
            if self.eat_kw(b"TO") {
                let (to, _) = self.nm_tok::<E>()?;
                return Ok(Ast::AlterRename { table, to });
            }
            // `COLUMN` is always the keyword here, as in the pin's
            // grammar (`RENAME column TO x` is a syntax error there).
            let _ = self.eat_kw(b"COLUMN");
            let (from, _) = self.nm_tok::<E>()?;
            self.expect_kw::<E>(b"TO")?;
            let (to, quoted) = self.nm_tok::<E>()?;
            return Ok(Ast::AlterRenameColumn {
                table,
                from,
                to,
                quoted,
            });
        }
        #[cfg(feature = "v16")]
        if self.eat_kw(b"DROP") {
            let _ = self.eat_kw(b"COLUMN");
            let (column, _) = self.nm_tok::<E>()?;
            return Ok(Ast::AlterDropColumn { table, column });
        }
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
        #[cfg(feature = "v16")]
        let mut fkeys = AVec::new();
        self.create_item::<E>(
            from,
            to,
            &mut columns,
            &mut pk_names,
            &mut pk_desc,
            &mut sets,
            &mut checks,
            #[cfg(feature = "v16")]
            &mut fkeys,
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
        let text = self.arena.alloc_copy_handle(trim(self.span(start, end)))?;
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
        // `createkw temp VIEW ifnotexists nm dbnm eidlist_opt AS
        // select`. The pin resolves the body only when the view is
        // read: its resolution depth is checked then (`parse_view`).
        self.defer = !self.view_use;
        self.reduced = true;
        let select = self.select_at::<E>(9);
        (self.defer, self.reduced) = (false, false);
        let select = select?;
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
    ///
    /// `held`: the pin's parser stack below the DEFAULT's operand
    /// (the parenthesised literal's sign run is held to the pin's
    /// limits there, v0.16 review fix: `DEFAULT (- - ... 1)` was
    /// stored past them, a schema the pin cannot load).
    fn default_clause<E>(
        &self,
        k: usize,
        to: usize,
        held: usize,
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
                    #[cfg(feature = "v04")]
                    Tok::HexTooBig => true,
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
                    self.sign_run::<E>(k + 1, n, held, 0)?;
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

    /// Consume one `nm` (v0.16, `v16`): its handle and whether it
    /// was quoted (a quoted identifier or a string).
    #[cfg(feature = "v16")]
    fn nm_tok<E>(&mut self) -> Result<(ABytes, bool), E> {
        if !self.nm_at(self.pos, self.limit) {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        }
        let t = self.cur();
        self.pos += 1;
        match t.tok {
            Tok::Ident(h, q) => Ok((h, q)),
            Tok::Text(h) => Ok((h, true)),
            _ => Err(perr(t.offset, ParseKind::UnexpectedToken)),
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
    /// parsed, not enforced). `from` is the child column count
    /// (one for a column constraint): a parent column list of
    /// another length is the pin's error (v0.16 fix — such a
    /// schema made the file unreadable to the pin).
    fn references<E>(&self, mut i: usize, to: usize, from: usize) -> Result<usize, E> {
        let bad = |j: usize| Err(perr(self.item_end_offset(j), ParseKind::UnexpectedToken));
        if !self.nm_at(i + 1, to) {
            return bad(i + 1);
        }
        i += 2;
        if i < to && matches!(self.toks.as_slice()[i].tok, Tok::LParen) {
            let (cols, next) = self.constraint_columns::<E>(i, to, false)?;
            if cols.len() != from {
                return Err(Error::Schema {
                    kind: crate::error::SchemaKind::ForeignKeyColumns,
                });
            }
            i = next;
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

    /// The foreign key a `REFERENCES` clause at token `i` declares
    /// (v0.16, `v16`; the clause was validated by `references`).
    /// `ON INSERT` actions parse and are dropped, as on the pin;
    /// `MATCH` is not recorded (the pin reports `NONE` always).
    #[cfg(feature = "v16")]
    fn fk_clause<E>(
        &self,
        from: AVec<ABytes>,
        mut i: usize,
        to: usize,
    ) -> Result<crate::sql::ast::ForeignKey, E> {
        let table = match self.toks.as_slice().get(i + 1).map(|t| t.tok) {
            Some(Tok::Ident(h, _) | Tok::Text(h)) => h,
            _ => {
                return Err(perr(
                    self.item_end_offset(i + 1),
                    ParseKind::UnexpectedToken,
                ));
            }
        };
        i += 2;
        let mut cols = AVec::new();
        if i < to && matches!(self.toks.as_slice()[i].tok, Tok::LParen) {
            let (c, next) = self.constraint_columns::<E>(i, to, false)?;
            for x in c.iter() {
                cols.push(x.name);
            }
            i = next;
        }
        let (mut on_update, mut on_delete) = (0u8, 0u8);
        loop {
            if self.is_kw(i, b"MATCH") {
                i += 2;
            } else if self.is_kw(i, b"ON") {
                let act = if self.is_kw(i + 2, b"SET") {
                    if self.is_kw(i + 3, b"NULL") { 2 } else { 3 }
                } else if self.is_kw(i + 2, b"CASCADE") {
                    4
                } else if self.is_kw(i + 2, b"RESTRICT") {
                    1
                } else {
                    0
                };
                if self.is_kw(i + 1, b"DELETE") {
                    on_delete = act;
                } else if self.is_kw(i + 1, b"UPDATE") {
                    on_update = act;
                }
                i += if act == 1 || act == 4 { 3 } else { 4 };
            } else {
                break;
            }
        }
        Ok(crate::sql::ast::ForeignKey {
            from,
            table,
            to: cols,
            on_update,
            on_delete,
            deferred: false,
        })
    }

    /// Every name of a constraint column list is one of the
    /// table's columns (the pin's "no such column" / "unknown
    /// column ... in foreign key definition"; `rowid` is not one).
    fn known_columns<E>(&self, columns: &[ColumnDef], cols: &[IndexColumnDef]) -> Result<(), E> {
        for c in cols {
            let n = handle_bytes(self.arena, c.name);
            if !columns
                .iter()
                .any(|d| eq_ci(handle_bytes(self.arena, d.name), n))
            {
                return Err(Error::Schema {
                    kind: crate::error::SchemaKind::UnknownColumn,
                });
            }
        }
        Ok(())
    }

    /// Skip an optional `[NOT] DEFERRABLE [INITIALLY DEFERRED |
    /// IMMEDIATE]` at token `i` (parsed, not enforced).
    /// Whether the deferrable clause at token `i` (`deferrable`'s
    /// grammar) makes its foreign key deferred: `DEFERRABLE
    /// INITIALLY DEFERRED` only (`NOT DEFERRABLE INITIALLY
    /// DEFERRED` is immediate, as on the pin).
    #[cfg(feature = "v16")]
    fn defers(&self, i: usize) -> bool {
        self.is_kw(i, b"DEFERRABLE")
            && self.is_kw(i + 1, b"INITIALLY")
            && self.is_kw(i + 2, b"DEFERRED")
    }

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
                #[cfg(feature = "expr_index")]
                expr: None,
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
        #[cfg(feature = "v16")] fkeys: &mut AVec<crate::sql::ast::ForeignKey>,
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
                        // A table-level key's DESC does not stop the
                        // rowid alias (the pin passes no sort order
                        // for `PRIMARY KEY (a DESC)`; v0.16 fix: hipp
                        // wrote an autoindex the pin calls orphan).
                        for c in cols.iter() {
                            pk_names.push(c.name);
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
                    let (cols, next) = self.constraint_columns::<E>(i + 2, to, false)?;
                    if !self.is_kw(next, b"REFERENCES") {
                        return bad(next);
                    }
                    i = self.references::<E>(next, to, cols.len())?;
                    // Each child column must be one of the table's
                    // (v0.16 fix; all columns precede the table
                    // constraints).
                    self.known_columns::<E>(columns.as_slice(), cols.as_slice())?;
                    #[cfg(feature = "v16")]
                    {
                        let mut from = AVec::new();
                        for c in cols.iter() {
                            from.push(c.name);
                        }
                        fkeys.push(self.fk_clause::<E>(from, next, to)?);
                        if let Some(f) = fkeys.as_mut_slice().last_mut() {
                            f.deferred = self.defers(i);
                        }
                    }
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
        #[cfg(feature = "v16")]
        let mut dflt_text: Option<ABytes> = None;
        // Column constraints (`carglist`), in any order.
        while i < to {
            if self.is_kw(i, b"CONSTRAINT") {
                if !self.nm_at(i + 1, to) {
                    return bad(i + 1);
                }
                i += 2;
            } else if self.is_kw(i, b"DEFAULT") {
                // The pin's stack below the operand: the bottom
                // entry, `create_table ( columnname carglist DEFAULT
                // (` for the first column (7, probed), two more
                // (`columnlist ,`) for a later one and for ALTER
                // TABLE ADD COLUMN's.
                // (ALTER TABLE exists with `v16`: its item follows
                // ADD [COLUMN], not `(`.)
                #[cfg(feature = "v16")]
                let first = columns.is_empty()
                    && from
                        .checked_sub(1)
                        .and_then(|j| self.toks.as_slice().get(j))
                        .is_some_and(|t| matches!(t.tok, Tok::LParen));
                #[cfg(not(feature = "v16"))]
                let first = columns.is_empty();
                let held = if first { 7 } else { 9 };
                let (d, next) = self.default_clause::<E>(i + 1, to, held)?;
                #[cfg(feature = "v16")]
                {
                    let paren = matches!(tok(i + 1), Some(Tok::LParen));
                    let (a, b) = if paren {
                        (i + 2, next - 1)
                    } else {
                        (i + 1, next)
                    };
                    let start = self.toks.as_slice().get(a).map_or(0, |t| t.offset);
                    let end = self.item_end_offset(b);
                    let text = trim(self.sql.get(start..end).unwrap_or_default());
                    dflt_text = Some(self.arena.alloc_copy_handle(text)?);
                }
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
                        let text = trim(self.span(start.min(end), end));
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
                // The pin's `sqlite3DeferForeignKey`: the table's most
                // recent foreign key, whichever column declared it.
                #[cfg(feature = "v16")]
                if let Some(f) = fkeys.as_mut_slice().last_mut() {
                    f.deferred = self.defers(i);
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
                #[cfg(feature = "v16")]
                {
                    let mut from = AVec::new();
                    from.push(col_name);
                    fkeys.push(self.fk_clause::<E>(from, i, to)?);
                }
                i = self.references::<E>(i, to, 1)?;
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
            let text = trim(self.span(start_off, end_off));
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
            #[cfg(feature = "v16")]
            dflt_text,
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
                #[cfg(feature = "expr_index")]
                expr: None,
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
                #[cfg(feature = "expr_index")]
                expr: None,
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
/// A row value (v0.16, `v16`; there are none without it).
#[cfg(feature = "v04")]
fn is_vector(e: &Expr) -> bool {
    #[cfg(feature = "v16")]
    {
        matches!(e, Expr::Vector(_))
    }
    #[cfg(not(feature = "v16"))]
    {
        let _ = e;
        false
    }
}

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
    ///
    /// `held` symbols sit below the first `nm EQ expr` value on the
    /// pin's parser stack; a `LP idlist RP EQ` value two more, and
    /// any later item two more again (`setlist COMMA`).
    fn set_list<E>(&mut self, held: usize) -> Result<AVec<(ABytes, Expr)>, E> {
        let mut assignments: AVec<(ABytes, Expr)> = AVec::new();
        loop {
            let at = held + if assignments.is_empty() { 0 } else { 2 };
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
                let e = self.item::<E>(at + 2)?;
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
            let e = self.item::<E>(at)?;
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
            } else if self.eat_kw(b"FAIL") {
                conflict = ConflictAction::Fail;
            } else if self.eat_kw(b"ROLLBACK") {
                // v16: the whole transaction rolls back too; before
                // it a failed statement ended the bracket anyway.
                #[cfg(feature = "v16")]
                {
                    conflict = ConflictAction::Rollback;
                }
            } else if !self.eat_kw(b"ABORT") {
                return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
            }
            // OR ABORT: the engine's default (abort the statement)
            // — nothing to record.
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
        // `with UPDATE orconf xfullname indexed_opt SET nm EQ expr`.
        let assignments = self.set_list::<E>(9)?;
        // "too many columns in set list" (UPDATE's only: the pin's
        // upsert SET takes any number).
        if assignments.len() > MAX_COLUMN {
            return Err(perr(self.cur().offset, ParseKind::TooManyColumns));
        }
        // FROM (v0.7): sources in SELECT shape, after SET and
        // before WHERE, exactly as SQLite orders the clauses
        // (`... SET setlist FROM`). The target is a table of the
        // join too.
        #[cfg(feature = "v07")]
        let from = self.update_from::<E>()?;
        #[cfg(not(feature = "v07"))]
        let from: AVec<FromSource> = AVec::new();
        if from.len() >= MAX_JOIN_TABLES {
            return Err(Error::Schema {
                kind: crate::error::SchemaKind::TooManyTables,
            });
        }
        // `... SET setlist from WHERE expr`.
        let filter = if self.eat_kw(b"WHERE") {
            Some(self.item::<E>(10)?)
        } else {
            None
        };
        // ORDER BY/LIMIT on UPDATE stay rejected (the pinned
        // oracle's build rejects them too); expect_end in parse()
        // catches any trailing input.
        #[cfg(feature = "v07")]
        let returning = self.returning_clause::<E>(if filter.is_some() { 14 } else { 12 })?;
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
        self.source_list::<E>(9)
    }

    fn delete_stmt<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"DELETE")?;
        self.expect_kw::<E>(b"FROM")?;
        let table = self.name::<E>()?;
        // `with DELETE FROM xfullname indexed_opt WHERE expr`.
        let filter = if self.eat_kw(b"WHERE") {
            Some(self.item::<E>(7)?)
        } else {
            None
        };
        #[cfg(feature = "v07")]
        let returning = self.returning_clause::<E>(if filter.is_some() { 11 } else { 9 })?;
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

/// Whether a frame offset is a constant expression as the pin's
/// parser sees one (v0.16, `window`; `sqlite3ExprIsConstant` before
/// name resolution): literals, bound parameters and operators over
/// them. Columns, calls (no function is known constant before
/// resolution) and subqueries are not.
#[cfg(feature = "window")]
fn offset_is_constant(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::HexTooBig(_) => true,
        // A bound parameter counts as a constant there.
        #[cfg(feature = "params")]
        Expr::Param(..) => true,
        Expr::Neg(x)
        | Expr::Pos(x)
        | Expr::Not(x)
        | Expr::BitNot(x)
        | Expr::IsNull(x, _)
        | Expr::Cast(x, _)
        | Expr::Collate(x, _) => offset_is_constant(x),
        Expr::Binary(_, a, b) | Expr::Like(a, b, _) => {
            offset_is_constant(a) && offset_is_constant(b)
        }
        Expr::Between(x, a, b, _) => {
            offset_is_constant(x) && offset_is_constant(a) && offset_is_constant(b)
        }
        Expr::InList(x, l, _) => offset_is_constant(x) && l.iter().all(offset_is_constant),
        Expr::Vector(l) => l.iter().all(offset_is_constant),
        Expr::Case {
            base,
            whens,
            otherwise,
        } => {
            base.as_deref().is_none_or(offset_is_constant)
                && otherwise.as_deref().is_none_or(offset_is_constant)
                && whens
                    .iter()
                    .all(|(w, t)| offset_is_constant(w) && offset_is_constant(t))
        }
        _ => false,
    }
}

// ===================== CREATE / DROP TRIGGER =====================
//
// v0.16, `triggers`: `CREATE TRIGGER [IF NOT EXISTS] [schema .] name
// [BEFORE | AFTER | INSTEAD OF] DELETE | INSERT | UPDATE [OF col, ...]
// ON table [FOR EACH ROW] [WHEN expr] BEGIN stmt; ... END`, with the
// pin's restrictions on the body: INSERT / REPLACE / UPDATE / DELETE /
// SELECT (a WITH clause only before a SELECT), no schema-qualified
// target, no RETURNING, no DEFAULT VALUES, no parameters (the
// `parse_params` refusal). `RAISE()` is legal only in a trigger.
#[cfg(feature = "triggers")]
impl<'a> Parser<'a> {
    fn create_trigger<E>(&mut self) -> Result<Ast, E> {
        use super::ast::{CreateTrigger, TrigEvent, TrigTime};
        self.expect_kw::<E>(b"CREATE")?;
        self.expect_kw::<E>(b"TRIGGER")?;
        let if_not_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"NOT")?;
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        // [schema .] name
        let (mut name, mut name_at) = match self.cur().tok {
            Tok::Ident(h, _) => (h, self.cur().offset),
            _ => return Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
        };
        self.pos += 1;
        let mut qualified = false;
        if self.eat(Tok::Dot) {
            let s = handle_bytes(self.arena, name);
            if !eq_ci(s, b"main") && !eq_ci(s, b"temp") {
                return Err(Error::Schema {
                    kind: crate::error::SchemaKind::UnknownDatabase,
                });
            }
            qualified = true;
            match self.cur().tok {
                Tok::Ident(h, _) => {
                    name = h;
                    name_at = self.cur().offset;
                    self.pos += 1;
                }
                _ => return Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
            }
        }
        let time = if self.eat_kw(b"BEFORE") {
            TrigTime::Before
        } else if self.eat_kw(b"AFTER") {
            TrigTime::After
        } else if self.eat_kw(b"INSTEAD") {
            self.expect_kw::<E>(b"OF")?;
            TrigTime::Instead
        } else {
            TrigTime::Before
        };
        let mut of_cols = AVec::new();
        let event = if self.eat_kw(b"DELETE") {
            TrigEvent::Delete
        } else if self.eat_kw(b"INSERT") {
            TrigEvent::Insert
        } else if self.eat_kw(b"UPDATE") {
            if self.eat_kw(b"OF") {
                loop {
                    match self.cur().tok {
                        Tok::Ident(h, _) => {
                            self.pos += 1;
                            of_cols.push(h);
                        }
                        _ => return Err(perr(self.cur().offset, ParseKind::UnexpectedToken)),
                    }
                    if !self.eat(Tok::Comma) {
                        break;
                    }
                }
            }
            TrigEvent::Update
        } else {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        };
        self.expect_kw::<E>(b"ON")?;
        // ON [schema .] table
        let (mut table_temp, mut table_main) = (false, false);
        if matches!(
            self.toks.as_slice().get(self.pos + 1).map(|t| t.tok),
            Some(Tok::Dot)
        ) && let Tok::Ident(h, _) = self.cur().tok
        {
            let s = handle_bytes(self.arena, h);
            if eq_ci(s, b"temp") {
                table_temp = true;
            } else if eq_ci(s, b"main") {
                table_main = true;
            }
        }
        let table = self.name::<E>()?;
        if self.eat_kw(b"FOR") {
            self.expect_kw::<E>(b"EACH")?;
            self.expect_kw::<E>(b"ROW")?;
        }
        let in0 = self.in_trig;
        self.in_trig = true;
        let r = self.trigger_rest::<E>();
        self.in_trig = in0;
        let (when, steps, end) = r?;
        Ok(Ast::CreateTrigger(Box::new(CreateTrigger {
            name,
            if_not_exists,
            qualified,
            time,
            event,
            of_cols,
            table,
            table_temp,
            table_main,
            when,
            steps,
            name_at,
            end,
        })))
    }

    /// One past the last byte of the token before the cursor.
    fn prev_end(&self) -> usize {
        let Some(t) = self
            .pos
            .checked_sub(1)
            .and_then(|i| self.toks.as_slice().get(i))
        else {
            return 0;
        };
        token_end(self.sql, t.offset)
    }

    /// `[WHEN expr] BEGIN stmt; ... END` (the WHEN span, the body
    /// spans, one past END).
    #[allow(clippy::type_complexity)]
    fn trigger_rest<E>(
        &mut self,
    ) -> Result<(Option<(usize, usize)>, AVec<(usize, usize)>, usize), E> {
        let when = if self.eat_kw(b"WHEN") {
            let a = self.cur().offset;
            self.item::<E>(10)?;
            Some((a, self.prev_end()))
        } else {
            None
        };
        self.expect_kw::<E>(b"BEGIN")?;
        let mut steps = AVec::new();
        loop {
            let a = self.cur().offset;
            self.trigger_step::<E>()?;
            let b = self.prev_end();
            self.expect::<E>(Tok::Semi)?;
            steps.push((a, b));
            if self.cur_is_kw(b"END") {
                let end = token_end(self.sql, self.cur().offset);
                self.pos += 1;
                return Ok((when, steps, end));
            }
        }
    }

    /// One body statement, with the pin's restrictions.
    fn trigger_step<E>(&mut self) -> Result<(), E> {
        let qualified_target = |p: &Self, mut i: usize| -> bool {
            // INSERT / REPLACE: the name after INTO; UPDATE [OR x]:
            // the next name; DELETE FROM: the next.
            if p.is_kw(i, b"OR") {
                i += 2;
            }
            if p.is_kw(i, b"INTO") || p.is_kw(i, b"FROM") {
                i += 1;
            }
            matches!(p.toks.as_slice().get(i + 1).map(|t| t.tok), Some(Tok::Dot))
        };
        let ast = if self.cur_is_kw(b"INSERT")
            || self.cur_is_kw(b"REPLACE")
            || self.cur_is_kw(b"UPDATE")
            || self.cur_is_kw(b"DELETE")
        {
            if qualified_target(self, self.pos + 1) {
                return Err(Error::Schema {
                    kind: crate::error::SchemaKind::TriggerQualifiedTarget,
                });
            }
            if self.cur_is_kw(b"INSERT") {
                self.insert::<E>(false)?
            } else if self.cur_is_kw(b"REPLACE") {
                self.replace_dispatch::<E>()?
            } else if self.cur_is_kw(b"UPDATE") {
                self.update_dispatch::<E>()?
            } else {
                self.delete_dispatch::<E>()?
            }
        } else if self.cur_is_kw(b"SELECT") || self.cur_is_kw(b"VALUES") || self.with_at(self.pos) {
            Ast::Select(self.select_rich::<E>()?)
        } else {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        };
        let returning = match &ast {
            Ast::Insert {
                returning, columns, ..
            } => {
                // `INSERT ... DEFAULT VALUES` (an empty target list):
                // not in the pin's trigger grammar.
                if columns.as_ref().is_some_and(|c| c.is_empty()) {
                    return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
                }
                !returning.is_empty()
            }
            Ast::Update { returning, .. } | Ast::Delete { returning, .. } => !returning.is_empty(),
            _ => false,
        };
        if returning {
            return Err(Error::Schema {
                kind: crate::error::SchemaKind::TriggerReturning,
            });
        }
        Ok(())
    }

    /// `RAISE(IGNORE)` / `RAISE(ROLLBACK | ABORT | FAIL, expr)`.
    #[inline(never)]
    fn expr_raise<E>(&mut self) -> Result<Expr, E> {
        use super::ast::RaiseKind;
        self.pos += 2; // RAISE (
        let kind = if self.eat_kw(b"IGNORE") {
            RaiseKind::Ignore
        } else if self.eat_kw(b"ROLLBACK") {
            RaiseKind::Rollback
        } else if self.eat_kw(b"ABORT") {
            RaiseKind::Abort
        } else if self.eat_kw(b"FAIL") {
            RaiseKind::Fail
        } else {
            return Err(perr(self.cur().offset, ParseKind::UnexpectedToken));
        };
        let (mut h, mut k) = (0usize, EK_CONST);
        let msg = if kind == RaiseKind::Ignore {
            None
        } else {
            self.expect::<E>(Tok::Comma)?;
            let m = self.operand_expr::<E>(5)?;
            (h, k) = (self.eh, self.ek);
            Some(Box::new(m))
        };
        self.expect::<E>(Tok::RParen)?;
        self.need::<E>(4)?;
        self.set_h::<E>(h + 1, k & EK_CONST)?;
        Ok(Expr::Raise(kind, msg, self.in_trig))
    }

    fn drop_trigger<E>(&mut self) -> Result<Ast, E> {
        self.expect_kw::<E>(b"DROP")?;
        self.expect_kw::<E>(b"TRIGGER")?;
        let if_exists = if self.eat_kw(b"IF") {
            self.expect_kw::<E>(b"EXISTS")?;
            true
        } else {
            false
        };
        let name = self.name::<E>()?;
        Ok(Ast::DropTrigger { name, if_exists })
    }
}

/// One past the last byte of the token starting at `off` of `sql`
/// (v0.16, `triggers`): an identifier / number / keyword run, a
/// quoted string or name, or a one- / two-byte operator.
#[cfg(feature = "triggers")]
pub fn token_end(sql: &[u8], off: usize) -> usize {
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
    if close != 0 {
        i += 1;
        while i < n {
            if sql[i] == close {
                if close != b']' && sql.get(i + 1) == Some(&close) {
                    i += 2;
                    continue;
                }
                return i + 1;
            }
            i += 1;
        }
        return n;
    }
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80;
    if word(c) {
        // A blob literal x'..' / X'..'.
        if (c == b'x' || c == b'X') && sql.get(off + 1) == Some(&b'\'') {
            return token_end(sql, off + 1);
        }
        while i < n && (word(sql[i]) || (sql[i] == b'.' && c.is_ascii_digit())) {
            // A number's exponent sign: 1e+5.
            if (sql[i] == b'e' || sql[i] == b'E')
                && c.is_ascii_digit()
                && matches!(sql.get(i + 1), Some(b'+') | Some(b'-'))
            {
                i += 2;
                continue;
            }
            i += 1;
        }
        return i;
    }
    // Operators: two-byte forms first.
    let two = sql.get(off..off + 2).unwrap_or(&[]);
    if matches!(
        two,
        b"<=" | b">=" | b"<>" | b"!=" | b"==" | b"||" | b"<<" | b">>" | b"->"
    ) {
        if sql.get(off..off + 3) == Some(b"->>") {
            return off + 3;
        }
        return off + 2;
    }
    off + 1
}

/// Parse one statement of a trigger program (v0.16, `triggers`):
/// a stored trigger's body statement, or `SELECT <when>`, with
/// `RAISE()` allowed.
#[cfg(feature = "triggers")]
pub fn parse_trigger_step<E>(sql: &[u8], arena: &Arena, lim: Limits) -> Result<Ast, E> {
    let toks = lex_checked::<E>(sql, arena)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    p.in_trig = true;
    let ast = p.statement::<E>()?;
    p.expect_end::<E>()?;
    Ok(ast)
}

/// `parse_trigger_step`, also returning the statement's tokens
/// (v0.16, `triggers`; ALTER TABLE edits trigger texts by them).
#[cfg(feature = "triggers")]
pub fn parse_trigger_step_tokens<E>(
    sql: &[u8],
    arena: &Arena,
    lim: Limits,
) -> Result<(Ast, AVec<Token>), E> {
    let toks = lex_checked::<E>(sql, arena)?;
    let mut p = Parser::new(toks, arena, sql, lim);
    p.in_trig = true;
    let ast = p.statement::<E>()?;
    p.expect_end::<E>()?;
    Ok((ast, p.toks))
}

/// `SELECT *[, extra ...] FROM table [AS alias] [WHERE filter]` as an
/// AST (v0.16, `triggers`): the rows an UPDATE / DELETE of a view
/// hands its INSTEAD OF triggers.
#[cfg(feature = "triggers")]
pub fn star_select(
    table: ABytes,
    alias: Option<ABytes>,
    filter: Option<Expr>,
    extra: AVec<Expr>,
    arena: &Arena,
) -> SelectStmt {
    let toks = AVec::new();
    let p = Parser::new(toks, arena, &[], Limits::PIN);
    let mut s = p.table_select(table, false);
    if let Some(f) = s.from.as_mut_slice().first_mut() {
        f.alias = alias;
    }
    s.filter = filter;
    for e in extra.iter() {
        s.items
            .push(SelectItem::Expr(e.clone(), None, ABytes::EMPTY));
    }
    s
}

#[cfg(test)]
mod tests {

    /// The pin's statement length limit (v0.16): a length is all
    /// the check reads, so the boundary is testable without a
    /// gigabyte of SQL.
    #[cfg(feature = "v04")]
    #[test]
    fn statement_length_limit() {
        assert!(too_long::<()>(MAX_SQL_LENGTH).is_ok());
        assert!(matches!(
            too_long::<()>(MAX_SQL_LENGTH + 1),
            Err(Error::Parse {
                kind: ParseKind::StatementTooLong,
                ..
            })
        ));
    }

    /// The ON / USING fold equals the left-deep AND tree the pin
    /// builds (v0.16), with and without a written WHERE.
    #[cfg(feature = "v04")]
    #[test]
    fn on_chain_heights() {
        // A reference: heights of ((w AND t1) AND t2) ... built
        // one node at a time.
        let tree = |w: Option<usize>, terms: &[usize]| {
            let mut h = w;
            for t in terms {
                h = Some(match h {
                    None => *t,
                    Some(x) => x.max(*t) + 1,
                });
            }
            h.unwrap_or(0)
        };
        let cases: [&[usize]; 6] = [&[], &[1], &[5, 1], &[1, 5], &[3, 9, 2, 2], &[7, 1, 1, 1, 6]];
        for terms in cases {
            let mut c = OnChain::default();
            for t in terms {
                c.push(*t, 0);
            }
            for w in [None, Some(1), Some(4), Some(12)] {
                assert_eq!(c.height(w), tree(w, terms), "{w:?} {terms:?}");
            }
        }
    }
    use super::*;
    use crate::error::Scope;

    fn kind(sql: &str) -> Option<ParseKind> {
        let arena = Arena::new(Scope::Statement, 0, 1 << 16);
        match parse::<()>(sql.as_bytes(), &arena, Limits::PIN) {
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
            let p = Parser::new(toks, &arena, sql.as_bytes(), Limits::PIN);
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
        #[cfg(not(feature = "v16"))]
        assert!(matches!(
            schema_text_start::<()>(auto, &arena),
            Err(Error::Unsupported)
        ));
        #[cfg(feature = "v16")]
        {
            assert!(schema_text_start::<()>(auto, &arena).is_ok());
            assert!(has_autoincrement::<()>(auto, &arena));
            assert!(!has_autoincrement::<()>(
                b"CREATE TABLE t(\"AUTOINCREMENT\" INTEGER PRIMARY KEY, b DEFAULT 'autoincrement')",
                &arena
            ));
        }
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
        }) = parse::<()>(sql.as_bytes(), &arena, Limits::PIN)
        else {
            panic!("parses");
        };
        assert_eq!(columns.as_slice()[0].coll, Some(Coll::NoCase));
        assert_eq!(columns.as_slice()[1].coll, Some(Coll::Rtrim));
        let set = &constraint_sets.as_slice()[0];
        assert_eq!(set.columns.len(), 2);
        assert_eq!(set.columns.as_slice()[0].coll, Some(Coll::Binary));
        assert!(set.columns.as_slice()[0].desc);
        let Ok(Ast::CreateIndex { columns, .. }) = parse::<()>(
            b"CREATE INDEX i ON t(a COLLATE \"NoCase\" DESC)",
            &arena,
            Limits::PIN,
        ) else {
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
                    parse::<()>(sql.as_bytes(), &arena, Limits::PIN),
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
        let e = parse_expr_text::<()>(b"'a' || -b COLLATE nocase", &arena, Limits::PIN)
            .expect("parses");
        let Expr::Binary(BinOp::Concat, _, r) = e else {
            panic!("concat on top");
        };
        assert!(matches!(*r, Expr::Collate(ref x, Coll::NoCase) if matches!(**x, Expr::Neg(_))));
        let e = parse_expr_text::<()>(b"x NOT GLOB 'a*'", &arena, Limits::PIN).expect("parses");
        let Expr::Not(call) = e else {
            panic!("NOT GLOB negates the call");
        };
        assert!(matches!(*call, Expr::Func { ref args, .. } if args.len() == 2));
        let e =
            parse_expr_text::<()>(b"x LIKE 'a%' ESCAPE '\\'", &arena, Limits::PIN).expect("parses");
        assert!(matches!(e, Expr::Func { ref args, .. } if args.len() == 3));
        assert!(parse_expr_text::<()>(b"x GLOB 'a' ESCAPE 'b'", &arena, Limits::PIN).is_err());
    }
}
