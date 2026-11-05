//! The FTS5 full-text query language (`fts5_expr.c` / `fts5parse.y`)
//! and its evaluation.
//!
//! Grammar (precedence lowest first): `OR`, `AND`, `NOT`, then
//! implicit AND between adjacent phrases (an `exprlist`), so `a OR b
//! c` is `a OR (b AND c)` and `a NOT b c` is `a NOT (b c)`. Phrases
//! are barewords or `"..."` strings joined by `+`, an optional `*`
//! prefix marker, `^` for the column's first token; `NEAR(p1 p2 ...
//! [, N])`; column filters `col :`, `{c1 c2} :`, `- col :`, `-{...} :`
//! before a phrase, a NEAR group or a parenthesised expression.
//! Keywords are the upper-case barewords `AND`, `OR`, `NOT` and
//! `NEAR` before `(`.
//!
//! Evaluation follows the pin's node iterators (`fts5ExprNodeNext_*`
//! / `Test_*`) over each leaf's matching rows, so the phrases whose
//! position lists a row exposes (to `bm25()`, `highlight()`, ...) are
//! the pin's: a phrase reports its positions at a row only when its
//! leaf is positioned there.

use super::config::{Config, is_bareword};
use super::{Detail, Hit, positions};
use crate::arena::AVec;
use alloc::boxed::Box;

/// Why a query is refused (the pin's messages, string-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprErr {
    /// "fts5: syntax error near ...", "unterminated string",
    /// "expected integer, got ...".
    Syntax,
    /// "no such column: X".
    NoColumn,
    /// "fts5: column queries are not supported (detail=none)".
    ColumnQuery,
    /// "fts5: phrase / NEAR queries are not supported (detail!=full)".
    PhraseQuery,
    /// "fts5 expression tree is too large (maximum depth 256)".
    TooDeep,
}

/// One token of a phrase.
#[derive(Debug, Clone)]
pub struct Term {
    pub tok: AVec<u8>,
    pub prefix: bool,
}

/// A phrase: its tokens, `^`, and the leaf it belongs to.
#[derive(Debug, Clone)]
pub struct Phrase {
    pub terms: AVec<Term>,
    pub first: bool,
    pub leaf: usize,
}

/// A NEAR group (a single phrase is a group of one).
#[derive(Debug, Clone)]
pub struct Leaf {
    pub phrases: AVec<usize>,
    pub near: i64,
    /// Column filter (sorted column numbers), if any.
    pub colset: Option<AVec<usize>>,
    /// A phrase without tokens: matches nothing.
    pub eof: bool,
}

/// An expression node.
#[derive(Debug, Clone)]
pub enum Node {
    Leaf(usize),
    And(AVec<Node>),
    Or(AVec<Node>),
    Not(Box<Node>, Box<Node>),
}

/// A parsed query.
#[derive(Debug, Clone)]
pub struct Expr {
    pub root: Node,
    pub phrases: AVec<Phrase>,
    pub leaves: AVec<Leaf>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tk {
    Lp,
    Rp,
    Lcp,
    Rcp,
    Colon,
    Comma,
    Plus,
    Star,
    Minus,
    Caret,
    Eof,
    And,
    Or,
    Not,
    /// A string (bareword, or quoted: dequoted), and whether quoted.
    Str(AVec<u8>, bool),
}

/// `fts5ExprGetToken` over the whole query.
fn lex(q: &[u8]) -> Result<AVec<Tk>, ExprErr> {
    let mut out = AVec::new();
    let mut i = 0;
    loop {
        while matches!(q.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            i += 1;
        }
        let Some(&c) = q.get(i) else {
            out.push(Tk::Eof);
            return Ok(out);
        };
        let t = match c {
            b'(' => Tk::Lp,
            b')' => Tk::Rp,
            b'{' => Tk::Lcp,
            b'}' => Tk::Rcp,
            b':' => Tk::Colon,
            b',' => Tk::Comma,
            b'+' => Tk::Plus,
            b'*' => Tk::Star,
            b'-' => Tk::Minus,
            b'^' => Tk::Caret,
            0 => {
                out.push(Tk::Eof);
                return Ok(out);
            }
            b'"' => {
                let mut s = AVec::new();
                let mut j = i + 1;
                loop {
                    match q.get(j) {
                        Some(b'"') if q.get(j + 1) == Some(&b'"') => {
                            s.push(b'"');
                            j += 2;
                        }
                        Some(b'"') => {
                            j += 1;
                            break;
                        }
                        Some(0) | None => return Err(ExprErr::Syntax),
                        Some(&b) => {
                            s.push(b);
                            j += 1;
                        }
                    }
                }
                i = j;
                out.push(Tk::Str(s, true));
                continue;
            }
            _ if is_bareword(c) => {
                let mut j = i;
                while q.get(j).is_some_and(|b| is_bareword(*b)) {
                    j += 1;
                }
                let w = &q[i..j];
                i = j;
                out.push(match w {
                    b"OR" => Tk::Or,
                    b"NOT" => Tk::Not,
                    b"AND" => Tk::And,
                    _ => {
                        let mut s = AVec::new();
                        s.extend_from_slice(w);
                        Tk::Str(s, false)
                    }
                });
                continue;
            }
            _ => return Err(ExprErr::Syntax),
        };
        out.push(t);
        i += 1;
    }
}

const MAX_DEPTH: usize = 256;

struct Parser<'a> {
    t: AVec<Tk>,
    p: usize,
    cfg: &'a Config,
    e: Expr,
    /// `bPhraseToAnd` (pattern queries on detail != full): a phrase
    /// of several tokens becomes an AND of one-token phrases.
    to_and: bool,
}

impl Parser<'_> {
    fn peek(&self, k: usize) -> &Tk {
        self.t.as_slice().get(self.p + k).unwrap_or(&Tk::Eof)
    }

    fn eat(&mut self, t: &Tk) -> bool {
        if self.peek(0) == t {
            self.p += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tk) -> Result<(), ExprErr> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(ExprErr::Syntax)
        }
    }

    fn or(&mut self) -> Result<Node, ExprErr> {
        let mut l = self.and()?;
        while self.eat(&Tk::Or) {
            let r = self.and()?;
            l = join(true, l, r);
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Node, ExprErr> {
        let mut l = self.not()?;
        while self.eat(&Tk::And) {
            let r = self.not()?;
            l = join(false, l, r);
        }
        Ok(l)
    }

    fn not(&mut self) -> Result<Node, ExprErr> {
        let mut l = self.unary()?;
        while self.eat(&Tk::Not) {
            let r = self.unary()?;
            l = Node::Not(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    /// Whether a colset starts here (a string or `-` string or `{`,
    /// ending in `:`).
    fn colset_ahead(&self) -> bool {
        match self.peek(0) {
            Tk::Str(..) => self.peek(1) == &Tk::Colon,
            Tk::Lcp => true,
            Tk::Minus => true,
            _ => false,
        }
    }

    fn unary(&mut self) -> Result<Node, ExprErr> {
        if self.eat(&Tk::Lp) {
            let n = self.or()?;
            self.expect(&Tk::Rp)?;
            return Ok(n);
        }
        if self.colset_ahead() {
            let save = self.p;
            let cs = self.colset()?;
            self.expect(&Tk::Colon)?;
            if self.eat(&Tk::Lp) {
                let n = self.or()?;
                self.expect(&Tk::Rp)?;
                self.set_colset(&n, &cs);
                return Ok(n);
            }
            self.p = save;
        }
        // exprlist: one or more cnearsets, implicitly ANDed.
        let mut l = self.cnearset()?;
        while matches!(self.peek(0), Tk::Str(..) | Tk::Caret | Tk::Minus | Tk::Lcp) {
            let r = self.cnearset()?;
            l = self.implicit_and(l, r);
        }
        Ok(l)
    }

    /// `sqlite3Fts5ParseImplicitAnd`: phrases without tokens drop out.
    fn implicit_and(&mut self, l: Node, r: Node) -> Node {
        let eof = |p: &Self, n: &Node| matches!(n, Node::Leaf(i) if p.e.leaves[*i].eof);
        if eof(self, &r) {
            return l;
        }
        let prev_eof = match &l {
            Node::And(c) => c.as_slice().last().is_some_and(|x| eof(self, x)),
            x => eof(self, x),
        };
        if prev_eof {
            return match l {
                Node::And(mut c) => {
                    c.pop();
                    c.push(r);
                    Node::And(c)
                }
                _ => r,
            };
        }
        join(false, l, r)
    }

    fn cnearset(&mut self) -> Result<Node, ExprErr> {
        if self.colset_ahead() {
            let cs = self.colset()?;
            self.expect(&Tk::Colon)?;
            let n = self.nearset()?;
            self.set_colset(&n, &cs);
            return Ok(n);
        }
        self.nearset()
    }

    /// `colset`: the column numbers (sorted, distinct).
    fn colset(&mut self) -> Result<AVec<usize>, ExprErr> {
        if self.cfg.detail == Detail::None {
            return Err(ExprErr::ColumnQuery);
        }
        let invert = self.eat(&Tk::Minus);
        let mut cols = AVec::new();
        let mut add = |p: &Self, s: &[u8]| -> Result<(), ExprErr> {
            let i = p
                .cfg
                .cols
                .iter()
                .position(|c| c.as_slice().eq_ignore_ascii_case(s))
                .ok_or(ExprErr::NoColumn)?;
            if let Err(k) = cols.as_slice().binary_search(&i) {
                cols.insert_at(k, i);
            }
            Ok(())
        };
        if self.eat(&Tk::Lcp) {
            let mut any = false;
            while let Tk::Str(s, _) = self.peek(0).clone() {
                add(self, s.as_slice())?;
                self.p += 1;
                any = true;
            }
            if !any {
                return Err(ExprErr::Syntax);
            }
            self.expect(&Tk::Rcp)?;
        } else if let Tk::Str(s, _) = self.peek(0).clone() {
            add(self, s.as_slice())?;
            self.p += 1;
        } else {
            return Err(ExprErr::Syntax);
        }
        if invert {
            let mut inv = AVec::new();
            for i in 0..self.cfg.cols.len() {
                if cols.as_slice().binary_search(&i).is_err() {
                    inv.push(i);
                }
            }
            cols = inv;
        }
        Ok(cols)
    }

    /// `fts5ParseSetColset` over every leaf under `n`.
    fn set_colset(&mut self, n: &Node, cs: &AVec<usize>) {
        match n {
            Node::Leaf(i) => {
                let lf = &mut self.e.leaves[*i];
                let merged = match &lf.colset {
                    None => cs.clone(),
                    Some(old) => {
                        let mut m = AVec::new();
                        for c in old.iter() {
                            if cs.as_slice().binary_search(c).is_ok() {
                                m.push(*c);
                            }
                        }
                        m
                    }
                };
                // Only a merge that leaves no column makes the leaf
                // EOF (`fts5ParseSetColset`); a filter of no columns
                // from the start just never matches.
                if merged.is_empty() && lf.colset.is_some() {
                    lf.eof = true;
                }
                lf.colset = Some(merged);
            }
            Node::And(c) | Node::Or(c) => {
                for x in c.iter() {
                    self.set_colset(x, cs);
                }
            }
            Node::Not(a, b) => {
                self.set_colset(a, cs);
                self.set_colset(b, cs);
            }
        }
    }

    fn nearset(&mut self) -> Result<Node, ExprErr> {
        let leaf = self.e.leaves.len();
        self.e.leaves.push(Leaf {
            phrases: AVec::new(),
            near: 10,
            colset: None,
            eof: false,
        });
        let is_near = matches!(self.peek(0), Tk::Str(_, _)) && self.peek(1) == &Tk::Lp;
        if is_near {
            if let Tk::Str(s, q) = self.peek(0)
                && (*q || s.as_slice() != b"NEAR")
            {
                return Err(ExprErr::Syntax);
            }
            self.p += 2;
            let mut n = 0;
            while matches!(self.peek(0), Tk::Str(..)) {
                self.phrase(leaf, false)?;
                n += 1;
                // `sqlite3Fts5ParseNearset`: a phrase without tokens
                // leaves a group that has another.
                let ps = self.e.leaves[leaf].phrases.clone();
                if ps.len() >= 2 {
                    let (a, b) = (ps[ps.len() - 2], ps[ps.len() - 1]);
                    let drop = if self.e.phrases[b].terms.is_empty() {
                        Some(b)
                    } else if self.e.phrases[a].terms.is_empty() {
                        Some(a)
                    } else {
                        None
                    };
                    if let Some(d) = drop {
                        self.e.phrases.remove_at(d);
                        let lp = &mut self.e.leaves[leaf].phrases;
                        let at = lp.iter().position(|x| *x == d).unwrap_or(0);
                        lp.remove_at(at);
                        for x in lp.as_mut_slice().iter_mut() {
                            if *x > d {
                                *x -= 1;
                            }
                        }
                    }
                }
            }
            if n == 0 {
                return Err(ExprErr::Syntax);
            }
            if self.eat(&Tk::Comma) {
                let Tk::Str(d, _) = self.peek(0).clone() else {
                    return Err(ExprErr::Syntax);
                };
                self.p += 1;
                let mut v = 0i32;
                for c in d.iter() {
                    if !c.is_ascii_digit() {
                        return Err(ExprErr::Syntax);
                    }
                    v = v.wrapping_mul(10).wrapping_add(i32::from(c - b'0'));
                }
                self.e.leaves[leaf].near = i64::from(v);
            }
            self.expect(&Tk::Rp)?;
            let multi = self.e.leaves[leaf].phrases.len() != 1;
            if self.cfg.detail != Detail::Full && multi {
                return Err(ExprErr::PhraseQuery);
            }
        } else {
            let caret = self.eat(&Tk::Caret);
            if !matches!(self.peek(0), Tk::Str(..)) {
                return Err(ExprErr::Syntax);
            }
            self.phrase(leaf, caret)?;
            // fts5ParsePhraseToAnd.
            let pi = self.e.phrases.len() - 1;
            if self.to_and && self.e.phrases[pi].terms.len() > 1 {
                let ph = self.e.phrases.pop().ok_or(ExprErr::Syntax)?;
                let mut kids = AVec::new();
                for (j, t) in ph.terms.into_iter().enumerate() {
                    let lf = if j == 0 {
                        leaf
                    } else {
                        self.e.leaves.push(Leaf {
                            phrases: AVec::new(),
                            near: 10,
                            colset: None,
                            eof: false,
                        });
                        self.e.leaves.len() - 1
                    };
                    let mut one = AVec::new();
                    one.push(self.e.phrases.len());
                    self.e.leaves[lf].phrases = one;
                    let mut terms = AVec::new();
                    terms.push(Term {
                        tok: t.tok,
                        prefix: false,
                    });
                    self.e.phrases.push(Phrase {
                        terms,
                        first: false,
                        leaf: lf,
                    });
                    kids.push(Node::Leaf(lf));
                }
                return Ok(Node::And(kids));
            }
        }
        if self.cfg.detail != Detail::Full {
            for &p in self.e.leaves[leaf].phrases.iter() {
                let ph = &self.e.phrases[p];
                if ph.terms.len() > 1 || (!ph.terms.is_empty() && ph.first) {
                    return Err(ExprErr::PhraseQuery);
                }
            }
        }
        if self.e.leaves[leaf]
            .phrases
            .iter()
            .any(|p| self.e.phrases[*p].terms.is_empty())
        {
            self.e.leaves[leaf].eof = true;
        }
        Ok(Node::Leaf(leaf))
    }

    /// `phrase`: strings joined by `+`, each tokenized as a query.
    fn phrase(&mut self, leaf: usize, caret: bool) -> Result<(), ExprErr> {
        let mut terms = AVec::new();
        loop {
            let Tk::Str(s, _) = self.peek(0).clone() else {
                return Err(ExprErr::Syntax);
            };
            self.p += 1;
            let star = self.eat(&Tk::Star);
            let before = terms.len();
            self.cfg.tok.tokenize(s.as_slice(), &mut |t, _, _| {
                let mut tok = AVec::new();
                tok.extend_from_slice(&t[..t.len().min(32768)]);
                terms.push(Term { tok, prefix: false });
            });
            if star
                && terms.len() > before
                && let Some(t) = terms.as_mut_slice().last_mut()
            {
                t.prefix = true;
            }
            if !self.eat(&Tk::Plus) {
                break;
            }
        }
        let i = self.e.phrases.len();
        self.e.phrases.push(Phrase {
            terms,
            first: caret,
            leaf,
        });
        self.e.leaves[leaf].phrases.push(i);
        Ok(())
    }
}

/// An AND / OR node of `l` and `r`, flattened as
/// `fts5ExprAddChildren` does.
fn join(or: bool, l: Node, r: Node) -> Node {
    let mut c = AVec::new();
    for n in [l, r] {
        match n {
            Node::Or(k) if or => {
                for x in k {
                    c.push(x);
                }
            }
            Node::And(k) if !or => {
                for x in k {
                    c.push(x);
                }
            }
            x => c.push(x),
        }
    }
    if or { Node::Or(c) } else { Node::And(c) }
}

fn height(n: &Node) -> usize {
    match n {
        Node::Leaf(_) => 1,
        Node::And(c) | Node::Or(c) => 1 + c.iter().map(height).max().unwrap_or(0),
        Node::Not(a, b) => 1 + height(a).max(height(b)),
    }
}

impl Expr {
    /// Parse query `q` against table `cfg`; a column MATCH (`col`)
    /// filters every phrase to that column.
    pub fn parse(q: &[u8], cfg: &Config, col: Option<usize>) -> Result<Expr, ExprErr> {
        Self::parse_as(q, cfg, col, false)
    }

    /// `sqlite3Fts5ExprPattern`: the query a LIKE (`glob` false) or
    /// GLOB pattern on column `col` of a trigram table stands for --
    /// each run of three or more characters between wildcards a
    /// phrase, all ANDed (on detail != full, token by token; detail=
    /// none drops the column filter). None when no run is that long.
    pub fn pattern(
        text: &[u8],
        glob: bool,
        col: usize,
        cfg: &Config,
    ) -> Result<Option<Expr>, ExprErr> {
        let spec: [u8; 3] = if glob { *b"*?[" } else { *b"_%\0" };
        let mut q = AVec::new();
        let n = text.len();
        let (mut i, mut first) = (0usize, 0usize);
        while i <= n {
            let c = text.get(i).copied();
            if c.is_none() || c.is_some_and(|c| spec.contains(&c) && c != 0) {
                let run = text.get(first..i).unwrap_or_default();
                if run.iter().filter(|b| *b & 0xC0 != 0x80).count() >= 3 {
                    q.push(b'"');
                    for &b in run {
                        q.push(b);
                        if b == b'"' {
                            q.push(b'"');
                        }
                    }
                    q.extend_from_slice(b"\" ");
                }
                if glob && c == Some(b'[') {
                    i += 2;
                    if text.get(i - 1) == Some(&b'^') {
                        i += 1;
                    }
                    while i < n && text.get(i) != Some(&b']') {
                        i += 1;
                    }
                }
                first = i + 1;
            }
            i += 1;
        }
        if q.is_empty() {
            return Ok(None);
        }
        let full = cfg.detail == Detail::Full;
        let col = (cfg.detail != Detail::None).then_some(col);
        Self::parse_as(q.as_slice(), cfg, col, !full).map(Some)
    }

    fn parse_as(q: &[u8], cfg: &Config, col: Option<usize>, to_and: bool) -> Result<Expr, ExprErr> {
        let t = lex(q)?;
        let mut p = Parser {
            t,
            p: 0,
            cfg,
            e: Expr {
                root: Node::Leaf(0),
                phrases: AVec::new(),
                leaves: AVec::new(),
            },
            to_and,
        };
        let root = p.or()?;
        if p.peek(0) != &Tk::Eof {
            return Err(ExprErr::Syntax);
        }
        if let Some(c) = col {
            if cfg.detail == Detail::None {
                return Err(ExprErr::ColumnQuery);
            }
            let mut cs = AVec::new();
            cs.push(c);
            p.set_colset(&root, &cs);
        }
        if height(&root) > MAX_DEPTH {
            return Err(ExprErr::TooDeep);
        }
        p.e.root = root;
        Ok(p.e)
    }

    /// `sqlite3Fts5ExprAnd`: both queries must match.
    pub fn and(mut self, other: Expr) -> Expr {
        let np = self.phrases.len();
        let nl = self.leaves.len();
        for mut ph in other.phrases {
            ph.leaf += nl;
            self.phrases.push(ph);
        }
        for mut lf in other.leaves {
            for p in lf.phrases.as_mut_slice().iter_mut() {
                *p += np;
            }
            self.leaves.push(lf);
        }
        let r = shift(other.root, nl);
        let l = core::mem::replace(&mut self.root, Node::Leaf(0));
        self.root = join(false, l, r);
        self
    }
}

fn shift(n: Node, by: usize) -> Node {
    match n {
        Node::Leaf(i) => Node::Leaf(i + by),
        Node::And(c) => Node::And(c.into_iter().map(|x| shift(x, by)).collect()),
        Node::Or(c) => Node::Or(c.into_iter().map(|x| shift(x, by)).collect()),
        Node::Not(a, b) => Node::Not(Box::new(shift(*a, by)), Box::new(shift(*b, by))),
    }
}

// ---------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------

/// A leaf's matching rows: per row, each phrase's positions (detail=
/// full: `col << 32 | offset`; column: column numbers; none: one 0).
#[derive(Debug, Clone, Default)]
pub struct LeafRows {
    /// Candidate rows (every token present), with each phrase's
    /// positions when the row matches (`None`: the pin's bNomatch).
    pub rows: AVec<(i64, Option<AVec<AVec<i64>>>)>,
}

/// The positions of a term's doclist at its rows, filtered to
/// `colset`.
fn term_rows(
    h: &AVec<Hit>,
    colset: Option<&AVec<usize>>,
    detail: Detail,
) -> AVec<(i64, AVec<i64>)> {
    let mut out = AVec::new();
    for x in h.iter() {
        let mut p = if detail == Detail::None {
            let mut v = AVec::new();
            v.push(0);
            v
        } else {
            positions(x.pl.as_slice())
        };
        if let Some(cs) = colset {
            let col = |v: i64| {
                if detail == Detail::Full {
                    (v >> 32) as usize
                } else {
                    v as usize
                }
            };
            let mut kept = AVec::new();
            for v in p.iter() {
                if cs.as_slice().binary_search(&col(*v)).is_ok() {
                    kept.push(*v);
                }
            }
            p = kept;
        }
        out.push((x.rowid, p));
    }
    out
}

/// `fts5ExprPhraseIsMatch`: where the phrase's terms line up.
fn phrase_match(tp: &[&AVec<i64>], first: bool) -> AVec<i64> {
    let mut out = AVec::new();
    let n = tp.len();
    let mut ix: AVec<usize> = AVec::new();
    for _ in 0..n {
        ix.push(0);
    }
    let at = |k: usize, ix: &AVec<usize>| tp[k].as_slice().get(ix[k]).copied();
    'outer: while let Some(mut ipos) = at(0, &ix) {
        loop {
            let mut ok = true;
            for k in 0..n {
                let adj = ipos + k as i64;
                let Some(mut cur) = at(k, &ix) else {
                    break 'outer;
                };
                if cur != adj {
                    ok = false;
                    while cur < adj {
                        ix[k] += 1;
                        match at(k, &ix) {
                            Some(c) => cur = c,
                            None => break 'outer,
                        }
                    }
                    if cur > adj {
                        ipos = cur - k as i64;
                    }
                }
            }
            if ok {
                break;
            }
        }
        if !first || ipos & 0xFFFF_FFFF == 0 {
            out.push(ipos);
        }
        for k in 0..n {
            ix[k] += 1;
            if at(k, &ix).is_none() {
                break 'outer;
            }
        }
    }
    out
}

const LOOKAHEAD_EOF: i64 = 1 << 62;

/// `fts5ExprNearIsMatch`: the phrases' positions within `near` of
/// each other, each phrase's list cut to the instances that are.
fn near_match(lists: &mut AVec<AVec<i64>>, ntok: &[i64], near: i64) -> bool {
    let n = lists.len();
    // (index of iPos, positions) per reader; iPos at idx, lookahead
    // at idx + 1.
    let mut idx: AVec<usize> = AVec::new();
    let mut outs: AVec<AVec<i64>> = AVec::new();
    for _ in 0..n {
        idx.push(0);
        outs.push(AVec::new());
    }
    let pos = |k: usize, idx: &AVec<usize>| {
        lists[k]
            .as_slice()
            .get(idx[k])
            .copied()
            .unwrap_or(LOOKAHEAD_EOF)
    };
    let look = |k: usize, idx: &AVec<usize>| {
        lists[k]
            .as_slice()
            .get(idx[k] + 1)
            .copied()
            .unwrap_or(LOOKAHEAD_EOF)
    };
    if (0..n).any(|k| lists[k].is_empty()) {
        return false;
    }
    'out: loop {
        let mut imax = pos(0, &idx);
        loop {
            let mut ok = true;
            for k in 0..n {
                let imin = imax - ntok[k] - near;
                let p = pos(k, &idx);
                if p < imin || p > imax {
                    ok = false;
                    while pos(k, &idx) < imin {
                        idx[k] += 1;
                        if pos(k, &idx) == LOOKAHEAD_EOF {
                            break 'out;
                        }
                    }
                    let p = pos(k, &idx);
                    if p > imax {
                        imax = p;
                    }
                }
            }
            if ok {
                break;
            }
        }
        for k in 0..n {
            let p = pos(k, &idx);
            if outs[k].as_slice().last() != Some(&p) {
                outs[k].push(p);
            }
        }
        let mut adv = 0;
        let mut m = look(0, &idx);
        for k in 0..n {
            if look(k, &idx) < m {
                m = look(k, &idx);
                adv = k;
            }
        }
        idx[adv] += 1;
        if pos(adv, &idx) == LOOKAHEAD_EOF {
            break;
        }
    }
    let ok = !outs[0].is_empty();
    *lists = outs;
    ok
}

/// The rows of each leaf of `e`: `lookup(key, prefix)` reads a
/// term's merged doclist (`key` is the `'0'`-prefixed token).
pub fn leaf_rows(
    e: &Expr,
    detail: Detail,
    prefix: &[u16],
    lookup: &mut dyn FnMut(&[u8], bool) -> AVec<Hit>,
) -> AVec<LeafRows> {
    let mut out = AVec::new();
    for lf in e.leaves.iter() {
        out.push(one_leaf(e, lf, detail, prefix, lookup, None));
    }
    out
}

/// One term's rows: rowid and positions.
type TermRows = AVec<(i64, AVec<i64>)>;

/// The rows phrase `ip` matches alone (with its group's column
/// filter): `xQueryPhrase`.
pub fn phrase_rows(
    e: &Expr,
    ip: usize,
    detail: Detail,
    prefix: &[u16],
    lookup: &mut dyn FnMut(&[u8], bool) -> AVec<Hit>,
) -> usize {
    let Some(ph) = e.phrases.as_slice().get(ip) else {
        return 0;
    };
    let lf = &e.leaves[ph.leaf];
    one_leaf(e, lf, detail, prefix, lookup, Some(ip))
        .rows
        .iter()
        .filter(|r| r.1.is_some())
        .count()
}

/// `fts5IndexCharlen`.
fn charlen(p: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < p.len() {
        let b = p[i];
        i += 1;
        if b >= 0xC0 {
            while p.get(i).is_some_and(|c| c & 0xC0 == 0x80) {
                i += 1;
            }
        }
        n += 1;
    }
    n
}

/// A term's doclist as `sqlite3Fts5IndexQuery` reads it: a prefix
/// query of a configured prefix length reads that prefix index, one a
/// character shorter the main index's exact term and the longer
/// prefix index; anything else the main index.
fn term_hits(
    t: &Term,
    prefix: &[u16],
    lookup: &mut dyn FnMut(&[u8], bool) -> AVec<Hit>,
) -> AVec<Hit> {
    let key = |b: u8| {
        let mut k = AVec::new();
        k.push(b);
        k.extend_from_slice(t.tok.as_slice());
        k
    };
    if t.prefix {
        let n = charlen(t.tok.as_slice());
        if let Some(i) = prefix.iter().position(|p| usize::from(*p) == n) {
            return lookup(key(b'1' + i as u8).as_slice(), false);
        }
        if let Some(i) = prefix.iter().position(|p| usize::from(*p) == n + 1) {
            let a = lookup(key(b'0').as_slice(), false);
            let b = lookup(key(b'1' + i as u8).as_slice(), true);
            return super::union_hits(&a, &b, Detail::Full);
        }
    }
    lookup(key(b'0').as_slice(), t.prefix)
}

fn one_leaf(
    e: &Expr,
    lf: &Leaf,
    detail: Detail,
    prefix: &[u16],
    lookup: &mut dyn FnMut(&[u8], bool) -> AVec<Hit>,
    only: Option<usize>,
) -> LeafRows {
    let mut res = LeafRows::default();
    let phrases: AVec<usize> = match only {
        Some(p) => core::iter::once(p).collect(),
        None => lf.phrases.clone(),
    };
    if lf.eof || phrases.iter().any(|p| e.phrases[*p].terms.is_empty()) {
        return res;
    }
    // Every term's rows.
    let mut terms: AVec<AVec<TermRows>> = AVec::new();
    for p in phrases.iter() {
        let mut ts = AVec::new();
        for t in e.phrases[*p].terms.iter() {
            let h = term_hits(t, prefix, lookup);
            ts.push(term_rows(&h, lf.colset.as_ref(), detail));
        }
        terms.push(ts);
    }
    // Candidate rows: in every term's doclist.
    let Some(first) = terms.as_slice().first().and_then(|t| t.as_slice().first()) else {
        return res;
    };
    let mut ix: AVec<AVec<usize>> = AVec::new();
    for t in terms.iter() {
        ix.push(t.iter().map(|_| 0usize).collect());
    }
    for (rowid, _) in first.iter() {
        let rowid = *rowid;
        let mut all = true;
        let mut cand = true;
        let mut lists: AVec<AVec<i64>> = AVec::new();
        for (pi, ts) in terms.iter().enumerate() {
            let mut tp: AVec<&AVec<i64>> = AVec::new();
            for (ti, t) in ts.iter().enumerate() {
                let k = &mut ix[pi][ti];
                while t.as_slice().get(*k).is_some_and(|x| x.0 < rowid) {
                    *k += 1;
                }
                match t.as_slice().get(*k) {
                    Some(x) if x.0 == rowid => tp.push(&x.1),
                    _ => {
                        all = false;
                        cand = false;
                        break;
                    }
                }
            }
            if !all {
                break;
            }
            let ph = &e.phrases[phrases[pi]];
            let simple = ph.terms.len() == 1 && !ph.first && lf.colset.is_none();
            let l = if simple || detail != Detail::Full {
                tp[0].clone()
            } else {
                phrase_match(tp.as_slice(), ph.first)
            };
            if l.is_empty() {
                all = false;
                break;
            }
            lists.push(l);
        }
        if !cand {
            continue;
        }
        if all && lists.len() > 1 {
            let ntok: AVec<i64> = phrases
                .iter()
                .map(|p| e.phrases[*p].terms.len() as i64)
                .collect();
            all = near_match(&mut lists, ntok.as_slice(), lf.near);
        }
        res.rows.push((rowid, all.then_some(lists)));
    }
    res
}

/// The iterator state of one node (the pin's `Fts5ExprNode`).
struct It {
    rowid: i64,
    eof: bool,
    nomatch: bool,
    /// Leaf: index into its candidate rows; zeroed by a NOT whose
    /// left side ended or a non-matching AND
    /// (`fts5ExprNodeZeroPoslist`).
    k: usize,
    zero: bool,
    kids: AVec<It>,
}

/// One row the query matched: its rowid and, per phrase, the
/// positions the row exposes (`None`: the phrase is not at the row).
#[derive(Debug, Clone)]
pub struct Row {
    pub rowid: i64,
    pub phrases: AVec<Option<AVec<i64>>>,
}

struct Eval<'a> {
    e: &'a Expr,
    leaves: &'a [LeafRows],
    desc: bool,
}

type Ord = core::cmp::Ordering;

impl Eval<'_> {
    fn cmp(&self, a: i64, b: i64) -> Ord {
        if self.desc { b.cmp(&a) } else { a.cmp(&b) }
    }

    /// `fts5NodeCompare`: an EOF node sorts last.
    fn node_cmp(&self, a: &It, b: &It) -> Ord {
        if b.eof {
            Ord::Less
        } else if a.eof {
            Ord::Greater
        } else {
            self.cmp(a.rowid, b.rowid)
        }
    }

    fn leaf_row(&self, li: usize, k: usize) -> Option<&(i64, Option<AVec<AVec<i64>>>)> {
        let rows = self.leaves[li].rows.as_slice();
        let j = if self.desc {
            rows.len().checked_sub(k + 1)?
        } else {
            k
        };
        rows.get(j)
    }

    fn new_it(&self, n: &Node) -> It {
        let kids = match n {
            Node::Leaf(_) => AVec::new(),
            Node::And(c) | Node::Or(c) => c.iter().map(|x| self.new_it(x)).collect(),
            Node::Not(a, b) => {
                let mut v = AVec::new();
                v.push(self.new_it(a));
                v.push(self.new_it(b));
                v
            }
        };
        It {
            rowid: 0,
            eof: false,
            nomatch: false,
            k: 0,
            zero: false,
            kids,
        }
    }

    fn leaf_set(&self, li: usize, it: &mut It) {
        it.zero = false;
        match self.leaf_row(li, it.k) {
            Some(r) => {
                it.rowid = r.0;
                it.nomatch = r.1.is_none();
            }
            None => {
                it.eof = true;
                it.nomatch = false;
            }
        }
    }

    fn first(&self, n: &Node, it: &mut It, root: bool) {
        it.eof = false;
        it.nomatch = false;
        match n {
            Node::Leaf(li) => {
                if self.e.leaves[*li].eof {
                    it.eof = true;
                    return;
                }
                it.k = 0;
                self.leaf_set(*li, it);
            }
            Node::And(c) | Node::Or(c) => {
                let mut neof = 0;
                for (x, k) in c.iter().zip(it.kids.as_mut_slice().iter_mut()) {
                    self.first(x, k, false);
                    neof += usize::from(k.eof);
                }
                it.rowid = it.kids[0].rowid;
                if matches!(n, Node::And(_)) {
                    if neof > 0 {
                        set_eof(it);
                    } else {
                        self.test_and(c, it, root);
                    }
                } else if neof == c.len() {
                    set_eof(it);
                } else {
                    self.test_or(it);
                }
            }
            Node::Not(a, b) => {
                self.first(a, &mut it.kids[0], false);
                self.first(b, &mut it.kids[1], false);
                it.rowid = it.kids[0].rowid;
                it.eof = it.kids[0].eof;
                if !it.eof {
                    self.test_not(a, b, it);
                }
            }
        }
    }

    /// `fts5ExprNodeNext`: advance, to `from` or later when given.
    fn next(&self, n: &Node, it: &mut It, from: Option<i64>, root: bool) {
        match n {
            Node::Leaf(li) => {
                if it.eof {
                    return;
                }
                it.k += 1;
                if let Some(f) = from {
                    while self
                        .leaf_row(*li, it.k)
                        .is_some_and(|r| self.cmp(r.0, f).is_lt())
                    {
                        it.k += 1;
                    }
                }
                self.leaf_set(*li, it);
            }
            Node::And(c) => {
                self.next(&c[0], &mut it.kids[0], from, false);
                self.test_and(c, it, root);
            }
            Node::Or(c) => {
                let last = it.rowid;
                for (x, k) in c.iter().zip(it.kids.as_mut_slice().iter_mut()) {
                    if !k.eof
                        && (k.rowid == last || from.is_some_and(|f| self.cmp(k.rowid, f).is_lt()))
                    {
                        self.next(x, k, from, false);
                    }
                }
                self.test_or(it);
            }
            Node::Not(a, b) => {
                self.next(a, &mut it.kids[0], from, false);
                self.test_not(a, b, it);
            }
        }
    }

    /// `fts5ExprNodeTest_AND`.
    fn test_and(&self, c: &AVec<Node>, it: &mut It, root: bool) {
        let mut last = it.rowid;
        loop {
            it.nomatch = false;
            let mut matched = true;
            for (x, k) in c.iter().zip(it.kids.as_mut_slice().iter_mut()) {
                if !k.eof && self.cmp(last, k.rowid).is_gt() {
                    self.next(x, k, Some(last), false);
                }
                if k.eof {
                    set_eof(it);
                    return;
                }
                if k.rowid != last {
                    matched = false;
                    last = k.rowid;
                }
                if k.nomatch {
                    it.nomatch = true;
                }
            }
            if matched {
                break;
            }
        }
        if it.nomatch && !root {
            zero(it);
        }
        it.rowid = last;
    }

    /// `fts5ExprNodeTest_OR`.
    fn test_or(&self, it: &mut It) {
        let mut best = 0usize;
        for i in 1..it.kids.len() {
            let c = self.node_cmp(&it.kids[best], &it.kids[i]);
            if c.is_gt() || (c.is_eq() && !it.kids[i].nomatch) {
                best = i;
            }
        }
        let b = &it.kids[best];
        it.rowid = b.rowid;
        it.eof = b.eof;
        it.nomatch = b.nomatch;
    }

    /// `fts5ExprNodeTest_NOT`.
    fn test_not(&self, a: &Node, b: &Node, it: &mut It) {
        let (l, r) = it.kids.as_mut_slice().split_at_mut(1);
        let (p1, p2) = (&mut l[0], &mut r[0]);
        while !p1.eof {
            let mut c = self.node_cmp(p1, p2);
            if c.is_gt() {
                self.next(b, p2, Some(p1.rowid), false);
                c = self.node_cmp(p1, p2);
            }
            if c.is_ne() || p2.nomatch {
                break;
            }
            self.next(a, p1, None, false);
        }
        it.eof = p1.eof;
        it.nomatch = p1.nomatch;
        it.rowid = p1.rowid;
        if p1.eof {
            zero(p2);
        }
    }

    /// The phrases leaf iterators under `it` expose at row `r`.
    fn collect(&self, n: &Node, it: &It, r: i64, out: &mut AVec<Option<AVec<i64>>>) {
        match n {
            Node::Leaf(li) => {
                if it.eof || it.zero || it.rowid != r {
                    return;
                }
                let Some((_, Some(lists))) = self.leaf_row(*li, it.k) else {
                    return;
                };
                for (p, l) in self.e.leaves[*li].phrases.iter().zip(lists.iter()) {
                    if let Some(slot) = out.as_mut_slice().get_mut(*p) {
                        *slot = Some(l.clone());
                    }
                }
            }
            Node::And(c) | Node::Or(c) => {
                for (x, k) in c.iter().zip(it.kids.iter()) {
                    self.collect(x, k, r, out);
                }
            }
            Node::Not(a, b) => {
                self.collect(a, &it.kids[0], r, out);
                self.collect(b, &it.kids[1], r, out);
            }
        }
    }
}

/// `fts5ExprSetEof`: the node and everything under it.
fn set_eof(it: &mut It) {
    it.eof = true;
    it.nomatch = false;
    for k in it.kids.as_mut_slice().iter_mut() {
        set_eof(k);
    }
}

fn zero(it: &mut It) {
    it.zero = true;
    for k in it.kids.as_mut_slice().iter_mut() {
        zero(k);
    }
}

/// Run `e` over its leaves' rows: the matching rows in rowid order
/// (descending when `desc`), each with its exposed phrase positions.
pub fn run(e: &Expr, leaves: &[LeafRows], desc: bool) -> AVec<Row> {
    let ev = Eval { e, leaves, desc };
    let mut it = ev.new_it(&e.root);
    ev.first(&e.root, &mut it, true);
    let mut out = AVec::new();
    loop {
        while !it.eof && it.nomatch {
            ev.next(&e.root, &mut it, None, true);
        }
        if it.eof {
            break;
        }
        let r = it.rowid;
        let mut ph: AVec<Option<AVec<i64>>> = AVec::new();
        for _ in 0..e.phrases.len() {
            ph.push(None);
        }
        ev.collect(&e.root, &it, r, &mut ph);
        out.push(Row {
            rowid: r,
            phrases: ph,
        });
        ev.next(&e.root, &mut it, None, true);
    }
    out
}
