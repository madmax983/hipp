//! The FTS5 auxiliary functions of the pin's `fts5_aux.c` —
//! `bm25()`, `highlight()`, `snippet()` — and the extension-API
//! pieces they read (`xInst`, `xColumnSize`, `xQueryPhrase`'s hit
//! counts), over one matched row.
//!
//! The session computes an [`AuxRow`] per row a full-text query
//! returns and stores it in the statement's environment; the row's
//! hidden table column holds its handle, which is the first argument
//! the functions take.

use super::Detail;
use super::config::Config;
use crate::arena::AVec;
use crate::record::OwnedValue;
use alloc::rc::Rc;

/// What every row of one full-text query shares.
#[derive(Debug, Clone)]
pub struct AuxTab {
    pub cfg: Config,
    /// Per phrase: its first token and whether it is a prefix, its
    /// token count, its column filter, and the rows it matches alone.
    pub first: AVec<(AVec<u8>, bool)>,
    pub psize: AVec<usize>,
    pub colsets: AVec<Option<AVec<usize>>>,
    pub nhit: AVec<i64>,
    /// `xRowCount` and `xColumnTotalSize(-1)`.
    pub nrow: i64,
    pub ntok: i64,
    /// No content to read (`content=''`).
    pub contentless: bool,
    /// The query, and whether its rows come through the rank sorter
    /// (ORDER BY rank): for detail=column / none the sorted rows'
    /// phrases are found in the text and checked against the query
    /// again (`sqlite3Fts5ExprCheckPoslists`).
    pub expr: Option<super::expr::Expr>,
    pub sorted: bool,
}

/// One matched row.
#[derive(Debug, Clone)]
pub struct AuxRow {
    pub tab: Rc<AuxTab>,
    /// Per phrase: the positions the row exposes, if any.
    pub phrases: AVec<Option<AVec<i64>>>,
    /// The column values (content).
    pub texts: AVec<OwnedValue>,
    /// Column sizes from `%_docsize` (`None`: count by tokenizing).
    pub sizes: Option<AVec<i64>>,
}

/// The statement's rows (`Env::fts_rows`).
pub type AuxRows = core::cell::RefCell<AVec<AuxRow>>;

/// `sqlite3_value_text`: a value's text (`None` for NULL).
pub fn text_of(v: &OwnedValue) -> Option<AVec<u8>> {
    let mut out = AVec::new();
    match v {
        OwnedValue::Null => return None,
        OwnedValue::Integer(i) => crate::record::render_int(*i, &mut out),
        OwnedValue::Real(r) => crate::record::render_real(*r, &mut out),
        OwnedValue::Text(t) | OwnedValue::Blob(t) => out.extend_from_slice(t.as_slice()),
    }
    Some(out)
}

/// Which auxiliary function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxFn {
    Bm25,
    Highlight,
    Snippet,
}

impl AuxFn {
    pub fn lookup(name: &[u8]) -> Option<AuxFn> {
        if name.eq_ignore_ascii_case(b"bm25") {
            Some(AuxFn::Bm25)
        } else if name.eq_ignore_ascii_case(b"highlight") {
            Some(AuxFn::Highlight)
        } else if name.eq_ignore_ascii_case(b"snippet") {
            Some(AuxFn::Snippet)
        } else {
            None
        }
    }
}

/// Why an auxiliary call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxErr {
    /// "wrong number of arguments to function highlight()" / ...
    Args,
    /// The first argument is no row of this statement's query.
    Context,
    /// A column index out of range (`SQLITE_RANGE`).
    Range,
    /// An instance past its column's size (`FTS5_CORRUPT`).
    Corrupt,
}

impl AuxRow {
    fn ncol(&self) -> usize {
        self.tab.cfg.cols.len()
    }

    fn text(&self, c: usize) -> Option<AVec<u8>> {
        self.texts.as_slice().get(c).and_then(text_of)
    }

    /// `xColumnSize(c)` (`c < 0`: the whole row).
    pub fn col_size(&self, c: i64) -> i64 {
        if c < 0 {
            return (0..self.ncol()).map(|i| self.col_size(i as i64)).sum();
        }
        let c = c as usize;
        if let Some(s) = &self.sizes {
            return s.as_slice().get(c).copied().unwrap_or(0);
        }
        if self.tab.cfg.unindexed.as_slice().get(c) == Some(&true) {
            return 0;
        }
        // Contentless without %_docsize: the pin reports -1.
        if self.tab.contentless {
            return -1;
        }
        let mut n = 0i64;
        if let Some(t) = self.text(c) {
            self.tab
                .cfg
                .tok
                .tokenize(t.as_slice(), &mut |_, _, _| n += 1);
        }
        n
    }

    /// `xInst`'s instances: (phrase, column, offset), in position
    /// order (`fts5CacheInstArray`).
    pub fn inst(&self) -> AVec<(usize, usize, i64)> {
        let mut lists: AVec<AVec<i64>> = AVec::new();
        if self.tab.cfg.detail == Detail::Full {
            for p in self.phrases.iter() {
                lists.push(p.clone().unwrap_or_default());
            }
        } else if !self.tab.contentless {
            // Detail column / none: the document tokenized again,
            // each live phrase's first token matched
            // (`sqlite3Fts5ExprPopulatePoslists`).
            for _ in self.phrases.iter() {
                lists.push(AVec::new());
            }
            let sorted = self.tab.sorted;
            for c in 0..self.ncol() {
                let Some(t) = self.text(c) else {
                    continue;
                };
                let mut off = ((c as i64) << 32) - 1;
                self.tab.cfg.tok.tokenize(t.as_slice(), &mut |tok, _, _| {
                    off += 1;
                    let tok = &tok[..tok.len().min(32768)];
                    for (ip, p) in self.phrases.iter().enumerate() {
                        if p.is_none() && !sorted {
                            continue;
                        }
                        if let Some(Some(cs)) = self.tab.colsets.as_slice().get(ip)
                            && cs.as_slice().binary_search(&c).is_err()
                        {
                            continue;
                        }
                        let Some((q, prefix)) = self.tab.first.as_slice().get(ip) else {
                            continue;
                        };
                        let hit = (q.len() == tok.len() || (q.len() < tok.len() && *prefix))
                            && tok.starts_with(q.as_slice());
                        if hit {
                            lists[ip].push(off);
                        }
                    }
                });
            }
            if sorted && let Some(e) = &self.tab.expr {
                check(e, &e.root, &mut lists);
            }
        }
        let mut out = AVec::new();
        let mut ix: AVec<usize> = lists.iter().map(|_| 0).collect();
        loop {
            let mut best: Option<usize> = None;
            for (i, l) in lists.iter().enumerate() {
                if let Some(p) = l.as_slice().get(ix[i])
                    && best.is_none_or(|b| *p < lists[b][ix[b]])
                {
                    best = Some(i);
                }
            }
            let Some(b) = best else {
                break;
            };
            let p = lists[b][ix[b]];
            out.push((b, (p >> 32) as usize, p & 0xFFFF_FFFF));
            ix[b] += 1;
        }
        out
    }

    fn psize(&self, ip: usize) -> i64 {
        self.tab.psize.as_slice().get(ip).copied().unwrap_or(0) as i64
    }
}

/// `fts5ExprCheckPoslists`: whether node `n` matches the row the
/// found positions describe; a failing AND / NOT clears its phrases.
fn check(e: &super::expr::Expr, n: &super::expr::Node, lists: &mut AVec<AVec<i64>>) -> bool {
    use super::expr::Node;
    let clear = |n: &Node, lists: &mut AVec<AVec<i64>>| {
        let mut leaves = AVec::new();
        leaves_of(n, &mut leaves);
        for li in leaves.iter() {
            for p in e.leaves[*li].phrases.iter() {
                if let Some(l) = lists.as_mut_slice().get_mut(*p) {
                    l.clear();
                }
            }
        }
    };
    match n {
        Node::Leaf(li) => e.leaves[*li]
            .phrases
            .as_slice()
            .first()
            .and_then(|p| lists.as_slice().get(*p))
            .is_some_and(|l| !l.is_empty()),
        Node::And(c) => {
            for x in c.iter() {
                if !check(e, x, lists) {
                    clear(n, lists);
                    return false;
                }
            }
            true
        }
        Node::Or(c) => {
            let mut any = false;
            for x in c.iter() {
                if check(e, x, lists) {
                    any = true;
                }
            }
            any
        }
        Node::Not(a, b) => {
            if !check(e, a, lists) || check(e, b, lists) {
                clear(n, lists);
                return false;
            }
            true
        }
    }
}

fn leaves_of(n: &super::expr::Node, out: &mut AVec<usize>) {
    use super::expr::Node;
    match n {
        Node::Leaf(i) => out.push(*i),
        Node::And(c) | Node::Or(c) => {
            for x in c.iter() {
                leaves_of(x, out);
            }
        }
        Node::Not(a, b) => {
            leaves_of(a, out);
            leaves_of(b, out);
        }
    }
}

/// `fts5Bm25Function`.
pub fn bm25(r: &AuxRow, w: &[f64]) -> f64 {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let t = &r.tab;
    let nrow = t.nrow as f64;
    let avgdl = t.ntok as f64 / nrow;
    let np = r.phrases.len();
    let mut freq: AVec<f64> = (0..np).map(|_| 0.0).collect();
    for (ip, ic, _) in r.inst().iter() {
        let wt = w.get(*ic).copied().unwrap_or(1.0);
        if let Some(f) = freq.as_mut_slice().get_mut(*ip) {
            *f += wt;
        }
    }
    let d = r.col_size(-1) as f64;
    let mut score = 0.0f64;
    for i in 0..np {
        let nhit = t.nhit.as_slice().get(i).copied().unwrap_or(0) as f64;
        let mut idf = crate::math::ln((nrow - nhit + 0.5) / (nhit + 0.5));
        if idf <= 0.0 {
            idf = 1e-6;
        }
        let f = freq[i];
        score += idf * ((f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * d / avgdl)));
    }
    -score
}

/// `CInstIter`: column `col`'s phrase instances, overlapping ones
/// coalesced.
struct CInst<'a> {
    inst: &'a [(usize, usize, i64)],
    r: &'a AuxRow,
    col: usize,
    i: usize,
    start: i64,
    end: i64,
}

impl CInst<'_> {
    fn next(&mut self) {
        self.start = -1;
        self.end = -1;
        while let Some(&(ip, ic, io)) = self.inst.get(self.i) {
            if ic == self.col {
                let e = io - 1 + self.r.psize(ip);
                if self.start < 0 {
                    self.start = io;
                    self.end = e;
                } else if io <= self.end {
                    if e > self.end {
                        self.end = e;
                    }
                } else {
                    break;
                }
            }
            self.i += 1;
        }
    }
}

/// `HighlightContext` and `fts5HighlightCb`.
struct Hl<'a> {
    it: CInst<'a>,
    range_start: i64,
    range_end: i64,
    open: &'a [u8],
    close: &'a [u8],
    zin: &'a [u8],
    iof: usize,
    pos: i64,
    bopen: bool,
    out: AVec<u8>,
}

impl Hl<'_> {
    fn app(&mut self, a: usize, b: usize) {
        if b > a {
            self.out
                .extend_from_slice(self.zin.get(a..b).unwrap_or(&[]));
        }
    }

    fn token(&mut self, s: usize, e: usize) {
        let ipos = self.pos;
        self.pos += 1;
        if self.range_end >= 0 {
            if ipos < self.range_start || ipos > self.range_end {
                return;
            }
            if self.range_start != 0 && ipos == self.range_start {
                self.iof = s;
            }
        }
        if self.bopen && (ipos <= self.it.start || self.it.start < 0) && s > self.iof {
            self.out.extend_from_slice(self.close);
            self.bopen = false;
        }
        if ipos == self.it.start && !self.bopen {
            self.app(self.iof, s);
            self.out.extend_from_slice(self.open);
            self.iof = s;
            self.bopen = true;
        }
        if ipos == self.it.end {
            if !self.bopen {
                self.out.extend_from_slice(self.open);
                self.bopen = true;
            }
            self.app(self.iof, e);
            self.iof = e.max(self.iof);
            self.it.next();
        }
        if ipos == self.range_end {
            if self.bopen {
                if self.it.start >= 0 && ipos >= self.it.start {
                    self.app(self.iof, e);
                    self.iof = e.max(self.iof);
                }
                self.out.extend_from_slice(self.close);
                self.bopen = false;
            }
            self.app(self.iof, e);
            self.iof = e.max(self.iof);
        }
    }
}

fn col_index(v: &OwnedValue) -> i64 {
    crate::func::int64_value(v) as i32 as i64
}

fn arg_text(v: Option<&OwnedValue>) -> AVec<u8> {
    v.and_then(text_of).unwrap_or_default()
}

/// `fts5HighlightFunction(col, open, close)`.
pub fn highlight(r: &AuxRow, args: &[OwnedValue]) -> Result<OwnedValue, AuxErr> {
    if args.len() != 3 {
        return Err(AuxErr::Args);
    }
    let ic = col_index(&args[0]);
    if ic < 0 || ic as usize >= r.ncol() {
        return Ok(OwnedValue::Text(AVec::new()));
    }
    let col = ic as usize;
    let Some(zin) = r.text(col) else {
        return Ok(OwnedValue::Null);
    };
    let open = arg_text(args.get(1));
    let close = arg_text(args.get(2));
    let inst = r.inst();
    let mut h = Hl {
        it: CInst {
            inst: inst.as_slice(),
            r,
            col,
            i: 0,
            start: -1,
            end: -1,
        },
        range_start: 0,
        range_end: -1,
        open: open.as_slice(),
        close: close.as_slice(),
        zin: zin.as_slice(),
        iof: 0,
        pos: 0,
        bopen: false,
        out: AVec::new(),
    };
    h.it.next();
    let tok = &r.tab.cfg.tok;
    tok.tokenize(zin.as_slice(), &mut |_, s, e| h.token(s, e));
    if h.bopen {
        h.out.extend_from_slice(h.close);
    }
    let n = zin.len();
    h.app(h.iof, n);
    Ok(OwnedValue::Text(h.out))
}

/// `fts5SnippetScore`.
fn snippet_score(
    r: &AuxRow,
    inst: &[(usize, usize, i64)],
    ndoc: i64,
    seen: &mut [bool],
    col: usize,
    ipos: i64,
    ntok: i64,
) -> (i64, i64) {
    let mut score = 0i64;
    let mut first = -1i64;
    let mut last = 0i64;
    let iend = ipos + ntok;
    for &(ip, ic, io) in inst {
        if ic == col && io >= ipos && io < iend {
            score += if seen.get(ip) == Some(&true) { 1 } else { 1000 };
            if let Some(s) = seen.get_mut(ip) {
                *s = true;
            }
            if first < 0 {
                first = io;
            }
            last = io + r.psize(ip);
        }
    }
    let mut adj = first - (ntok - (last - first)) / 2;
    if adj + ntok > ndoc {
        adj = ndoc - ntok;
    }
    if adj < 0 {
        adj = 0;
    }
    (score, adj)
}

/// `fts5SnippetFunction(col, open, close, ellipsis, ntoken)`.
pub fn snippet(r: &AuxRow, args: &[OwnedValue]) -> Result<OwnedValue, AuxErr> {
    if args.len() != 5 {
        return Err(AuxErr::Args);
    }
    let ncol = r.ncol();
    let icol = col_index(&args[0]);
    let open = arg_text(args.get(1));
    let close = arg_text(args.get(2));
    let ell = arg_text(args.get(3));
    let ntok = col_index(&args[4]);
    let mut best_col = if icol >= 0 { icol } else { 0 };
    let nphrase = r.phrases.len();
    let inst = r.inst();
    let mut best_start = 0i64;
    let mut best_score = 0i64;
    let mut col_size = 0i64;
    let mut seen: AVec<bool> = (0..nphrase).map(|_| false).collect();
    // The pin's corrupt flag stops the column's instance scan, and the
    // next column's xColumnText overwrites it: only the last column
    // scanned reports it.
    let mut bad = false;
    for i in 0..ncol {
        if icol >= 0 && icol as usize != i {
            continue;
        }
        bad = false;
        let doc = r.text(i).unwrap_or_default();
        // fts5SentenceFinderCb: the token positions that start a
        // sentence.
        let mut firsts: AVec<i64> = AVec::new();
        let mut p = 0i64;
        r.tab.cfg.tok.tokenize(doc.as_slice(), &mut |_, s, _| {
            if p > 0 {
                let mut j = s;
                let mut c = 0u8;
                while j > 0 {
                    c = doc[j - 1];
                    if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    j -= 1;
                }
                if j != s && (c == b'.' || c == b':') {
                    firsts.push(p);
                }
            } else {
                firsts.push(0);
            }
            p += 1;
        });
        let ndoc = r.col_size(i as i64);
        for &(_, ic, io) in inst.iter() {
            if ic != i {
                continue;
            }
            if io > ndoc {
                bad = true;
                break;
            }
            for s in seen.as_mut_slice().iter_mut() {
                *s = false;
            }
            let (score, adj) =
                snippet_score(r, inst.as_slice(), ndoc, seen.as_mut_slice(), i, io, ntok);
            if score > best_score {
                best_score = score;
                best_col = i as i64;
                best_start = adj;
                col_size = ndoc;
            }
            if !firsts.is_empty() && ndoc > ntok {
                let mut jj = 0usize;
                while jj + 1 < firsts.len() {
                    if firsts[jj + 1] > io {
                        break;
                    }
                    jj += 1;
                }
                if firsts[jj] < io {
                    for s in seen.as_mut_slice().iter_mut() {
                        *s = false;
                    }
                    let (mut score, _) = snippet_score(
                        r,
                        inst.as_slice(),
                        ndoc,
                        seen.as_mut_slice(),
                        i,
                        firsts[jj],
                        ntok,
                    );
                    score += if firsts[jj] == 0 { 120 } else { 100 };
                    if score > best_score {
                        best_score = score;
                        best_col = i as i64;
                        best_start = firsts[jj];
                        col_size = ndoc;
                    }
                }
            }
        }
    }
    if bad {
        return Err(AuxErr::Corrupt);
    }
    if best_col < 0 || best_col as usize >= ncol {
        return Err(AuxErr::Range);
    }
    let bc = best_col as usize;
    let Some(zin) = r.text(bc) else {
        return Ok(OwnedValue::Text(AVec::new()));
    };
    if col_size == 0 {
        col_size = r.col_size(best_col);
    }
    let mut h = Hl {
        it: CInst {
            inst: inst.as_slice(),
            r,
            col: bc,
            i: 0,
            start: -1,
            end: -1,
        },
        range_start: best_start,
        range_end: best_start + ntok - 1,
        open: open.as_slice(),
        close: close.as_slice(),
        zin: zin.as_slice(),
        iof: 0,
        pos: 0,
        bopen: false,
        out: AVec::new(),
    };
    h.it.next();
    if best_start > 0 {
        h.out.extend_from_slice(ell.as_slice());
    }
    while h.it.start >= 0 && h.it.start < best_start {
        h.it.next();
    }
    r.tab
        .cfg
        .tok
        .tokenize(zin.as_slice(), &mut |_, s, e| h.token(s, e));
    if h.bopen {
        h.out.extend_from_slice(h.close);
    }
    if h.range_end >= col_size - 1 {
        let n = zin.len();
        h.app(h.iof, n);
    } else {
        h.out.extend_from_slice(ell.as_slice());
    }
    Ok(OwnedValue::Text(h.out))
}

/// Call `f` with `args` (the first is the row handle, already
/// resolved to `row`).
pub fn call(f: AuxFn, row: &AuxRow, args: &[OwnedValue]) -> Result<OwnedValue, AuxErr> {
    match f {
        AuxFn::Bm25 => {
            let mut w = AVec::new();
            for a in args.iter() {
                w.push(crate::func::real_value(a));
            }
            Ok(OwnedValue::Real(bm25(row, w.as_slice())))
        }
        AuxFn::Highlight => highlight(row, args),
        AuxFn::Snippet => snippet(row, args),
    }
}
