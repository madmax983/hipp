//! Virtual tables (v0.16, leaf `vtab`): one internal module
//! abstraction modelled on the pin's `sqlite3_module`, as Rust
//! traits (no C ABI), and the modules that need no storage of their
//! own beyond the file: `generate_series` (as the pin's shell ships
//! it) and `dbstat` (`SQLITE_ENABLE_DBSTAT_VTAB`). The R*Tree module
//! (`rtree`) lives in `crate::rtree`.
//!
//! The pieces of `sqlite3_module` and where they are:
//!
//! - **xCreate / xConnect** — [`Vtab::connect`]: the module reads its
//!   `CREATE VIRTUAL TABLE` arguments and declares its columns
//!   ([`VTab`], [`VCol`]: names, declared types, HIDDEN). xCreate's
//!   storage (rtree's shadow tables) is made by the session layer.
//! - **xBestIndex** — [`Vtab::best_index`] over an [`IndexInfo`]: the
//!   usable WHERE / ON / table-valued-function constraints
//!   (`aConstraint`: column, [`Op`], usable) and the ORDER BY
//!   (`aOrderBy`) in; per constraint the `argvIndex` / `omit` usage,
//!   `idxNum`, `idxStr`, `orderByConsumed`, `estimatedCost` and
//!   `estimatedRows` out. The planner passes each used constraint's
//!   right-hand side to the filter in `argvIndex` order and drops an
//!   omitted term from the WHERE (the module checks it itself, with
//!   its own semantics: `value > '3'` is a number to
//!   `generate_series`).
//! - **xFilter / xNext / xEof / xColumn / xRowid** — [`VtCursor`]:
//!   a cursor positioned by `filter(idxNum, idxStr, argv)`, stepped
//!   by `next`, read by `column` / `rowid` until `eof`.
//!   `generate_series` streams ([`SeriesCursor`]); `dbstat` and the
//!   R*Tree fill a [`RowsCursor`] from the file first (their walks
//!   read pages through the pager).
//! - **xUpdate** — the session layer's virtual-table writes
//!   (`session::vtab`, `session::rtree`): `(old rowid, new row)`
//!   per row,
//!   the statement's conflict mode as `sqlite3_vtab_on_conflict`;
//!   only the R*Tree is writable ("table X may not be modified"
//!   otherwise).
//! - **xBegin / xSync / xCommit / xRollback / xSavepoint /
//!   xRelease / xRollbackTo** — the modules here keep no state
//!   outside the database file: every write goes through the pager
//!   in the statement's own transaction, so the pager's journal,
//!   statement savepoint and savepoints are the hooks (nothing is
//!   cached across statements).
//! - **Hidden columns, eponymous modules** — [`VCol::hidden`] (not
//!   in `*`, a table-valued function's arguments bind to them in
//!   order), [`Vtab::eponymous`] (usable by name with no CREATE:
//!   `generate_series`, `dbstat`), [`Vtab::creatable`] (has xCreate).
//! - **xShadowName** — [`Vtab::shadow_name`]: the module's shadow
//!   tables (`%_node`, `%_parent`, `%_rowid`), read only as under
//!   the pin shell's defensive mode.
//! - **xIntegrity** — the R*Tree's check is `rtreecheck()`
//!   (`crate::rtree::check`); PRAGMA integrity_check checks the
//!   shadow tables as tables, not the tree.

use crate::arena::AVec;
use crate::error::SchemaKind;
use crate::record::{Affinity, OwnedValue};

/// A virtual-table module hipp has (the pin's registered modules of
/// the same names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    /// `generate_series` (eponymous only, as in the pin's shell).
    Series,
    /// `dbstat` (eponymous, and creatable with an optional schema
    /// argument).
    Dbstat,
    /// `rtree` (`int: false`, 32-bit float coordinates) or
    /// `rtree_i32` (`int: true`), leaf `rtree`.
    #[cfg(feature = "rtree")]
    Rtree { int: bool },
    /// The one-row source `rtreecheck(...)` reads as (internal: its
    /// name cannot be written in SQL), leaf `rtree`.
    #[cfg(feature = "rtree")]
    Check,
    /// `fts5` (leaf `fts5`).
    #[cfg(feature = "fts5")]
    Fts5,
    /// `fts5vocab` (leaf `fts5`).
    #[cfg(feature = "fts5")]
    Fts5Vocab,
}

/// The internal name `rtreecheck()` calls resolve to (a FROM source
/// no SQL text can name: it holds a NUL).
#[cfg(feature = "rtree")]
pub const CHECK_NAME: &[u8] = b"\0rtreecheck";

/// One declared column of a virtual table (`sqlite3_declare_vtab`).
#[derive(Debug, Clone)]
pub struct VCol {
    pub name: AVec<u8>,
    /// The declared type (`INT`, `REAL`, `TEXT`, ... or empty).
    pub decl: &'static [u8],
    /// HIDDEN: not in `*`; a table-valued function's arguments bind
    /// to the hidden columns in order.
    pub hidden: bool,
}

impl VCol {
    fn new(name: &[u8], decl: &'static [u8], hidden: bool) -> VCol {
        let mut n = AVec::new();
        n.extend_from_slice(name);
        VCol {
            name: n,
            decl,
            hidden,
        }
    }

    /// The column's affinity, from its declared type.
    pub fn affinity(&self) -> Affinity {
        crate::record::affinity_of(self.decl)
    }
}

/// A connected virtual table: its module and declared columns
/// (xConnect's result), plus what the module read from its
/// arguments.
#[derive(Debug, Clone)]
pub struct VTab {
    pub module: Module,
    pub cols: AVec<VCol>,
    /// R*Tree: coordinates (2 per dimension).
    pub ndim2: u8,
    /// R*Tree: auxiliary `+columns`.
    pub naux: u8,
    /// dbstat: the schema argument (`dbstat(aux)`), empty for main.
    pub schema: AVec<u8>,
    /// fts5: the table definition (leaf `fts5`).
    #[cfg(feature = "fts5")]
    pub fts: Option<alloc::boxed::Box<crate::fts5::config::Config>>,
    /// fts5vocab: the fts5 table and the type (0 row, 1 col, 2
    /// instance).
    #[cfg(feature = "fts5")]
    pub vocab: Option<alloc::boxed::Box<(AVec<u8>, u8)>>,
}

impl VTab {
    /// The columns `*` reads (hidden ones are always last here).
    pub fn visible(&self) -> usize {
        self.cols.iter().take_while(|c| !c.hidden).count()
    }

    /// The hidden columns' indexes, in order (where a table-valued
    /// function's arguments go).
    pub fn hidden_cols(&self) -> AVec<usize> {
        let mut out = AVec::new();
        for (i, c) in self.cols.iter().enumerate() {
            if c.hidden {
                out.push(i);
            }
        }
        out
    }
}

/// A virtual-table FROM source as planned (`SourcePlan::vt`): the
/// connected table, its name (an R*Tree reads its shadow tables by
/// it; empty for an eponymous module) and xBestIndex's choice. The
/// filter's arguments, in `argvIndex` order, are the resolved
/// expressions of the source's placeholder plan (`values[0]`).
#[derive(Debug, Clone)]
pub struct VtSrc {
    pub tab: VTab,
    pub name: AVec<u8>,
    pub idx_num: i32,
    pub idx_str: AVec<u8>,
    /// Table-valued-function arguments written (they bind to the
    /// hidden columns, in order).
    pub nargs: usize,
    /// Whether the statement reads the table's content columns (the
    /// pin's `colUsed`; leaf `fts5`: a missing external-content row
    /// is an error only then).
    #[cfg(feature = "fts5")]
    pub content_used: bool,
    /// A WHERE term `a OR b OR ...` whose branches are each full-text
    /// constraints (MATCH, `t = q`, `rowid =`; leaf `fts5`): one
    /// filter per branch (idxNum, idxStr, arguments, and the rowid
    /// comparisons to recheck: argument index and operator), its rows
    /// unioned in branch order, each
    /// rowid once (the pin's multi-index OR).
    #[cfg(feature = "fts5")]
    pub ors: AVec<OrBranch>,
}

/// One branch of [`VtSrc::ors`].
#[cfg(feature = "fts5")]
pub type OrBranch = (i32, AVec<u8>, AVec<crate::query::RExpr>, AVec<(usize, Op)>);

/// A constraint operator (`SQLITE_INDEX_CONSTRAINT_*`): the ones the
/// planner hands modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Gt,
    Le,
    Lt,
    Ge,
    Is,
    /// MATCH (leaf `fts5`).
    #[cfg(feature = "fts5")]
    Match,
    /// `x LIKE p` (two arguments; leaf `fts5`, only offered to fts5
    /// tables).
    #[cfg(feature = "fts5")]
    Like,
    /// `x GLOB p` (leaf `fts5`, as Like).
    #[cfg(feature = "fts5")]
    Glob,
}

/// One `aConstraint` entry: column (-1 for the rowid), operator,
/// usable.
#[derive(Debug, Clone, Copy)]
pub struct Cons {
    pub col: i32,
    pub op: Op,
    pub usable: bool,
}

/// One `aConstraintUsage` entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    /// 1-based position of the constraint's value among the filter's
    /// arguments; 0: not passed.
    pub argv: u16,
    /// The module checks the constraint itself: the planner drops
    /// the term.
    pub omit: bool,
}

/// `sqlite3_index_info`.
#[derive(Debug, Clone, Default)]
pub struct IndexInfo {
    pub cons: AVec<Cons>,
    /// `aOrderBy`: (column, desc).
    pub order: AVec<(i32, bool)>,
    pub usage: AVec<Usage>,
    pub idx_num: i32,
    pub idx_str: AVec<u8>,
    pub consumed: bool,
    pub cost: f64,
    pub rows: i64,
}

impl IndexInfo {
    /// An index info for `cons` (and ORDER BY `order`), outputs
    /// cleared as the pin clears them.
    pub fn new(cons: AVec<Cons>, order: AVec<(i32, bool)>) -> IndexInfo {
        let mut usage = AVec::new();
        for _ in cons.iter() {
            usage.push(Usage::default());
        }
        IndexInfo {
            cons,
            order,
            usage,
            idx_num: 0,
            idx_str: AVec::new(),
            consumed: false,
            cost: 0.0,
            rows: 25,
        }
    }

    fn set(&mut self, i: usize, argv: u16, omit: bool) {
        if let Some(u) = self.usage.as_mut_slice().get_mut(i) {
            *u = Usage { argv, omit };
        }
    }

    /// How many arguments the filter takes (the largest argvIndex).
    pub fn nargs(&self) -> usize {
        self.usage
            .iter()
            .map(|u| usize::from(u.argv))
            .max()
            .unwrap_or(0)
    }
}

/// What `xBestIndex` / `xConnect` can say beyond success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BestErr {
    /// `SQLITE_CONSTRAINT`: this plan is unusable (an argument the
    /// module needs reads a source it cannot see).
    Unusable,
    /// An error statement (the module's message, string-free).
    Err(SchemaKind),
}

/// The module interface (`sqlite3_module`'s synchronous half; the
/// cursor half is [`VtCursor`], the writes `session::vtab`).
pub trait Vtab {
    /// xCreate (`create`) / xConnect: the declared table of a
    /// virtual table named `name` with module arguments `args`.
    fn connect(&self, name: &[u8], args: &[&[u8]], create: bool) -> Result<VTab, SchemaKind>;
    /// xBestIndex.
    fn best_index(&self, tab: &VTab, info: &mut IndexInfo) -> Result<(), BestErr>;
    /// Usable by its own name as a table-valued function.
    fn eponymous(&self) -> bool;
    /// Has xCreate: `CREATE VIRTUAL TABLE ... USING` it works.
    fn creatable(&self) -> bool;
    /// Has xUpdate.
    fn writable(&self) -> bool;
    /// xShadowName: whether `suffix` names one of its shadow tables.
    fn shadow_name(&self, suffix: &[u8]) -> bool;
}

/// A cursor (`sqlite3_vtab_cursor` with xNext / xEof / xColumn /
/// xRowid), positioned by its module's filter.
pub trait VtCursor {
    fn eof(&self) -> bool;
    fn next(&mut self);
    fn column(&self, i: usize) -> OwnedValue;
    fn rowid(&self) -> i64;
}

impl Module {
    /// The module a `USING` name or a FROM name calls (any case).
    pub fn lookup(name: &[u8]) -> Option<Module> {
        let eq = crate::sql::parser::eq_ci;
        if eq(name, b"generate_series") {
            Some(Module::Series)
        } else if eq(name, b"dbstat") {
            Some(Module::Dbstat)
        } else {
            #[cfg(feature = "rtree")]
            if eq(name, b"rtree") {
                return Some(Module::Rtree { int: false });
            } else if eq(name, b"rtree_i32") {
                return Some(Module::Rtree { int: true });
            } else if name == CHECK_NAME {
                return Some(Module::Check);
            }
            #[cfg(feature = "fts5")]
            if eq(name, b"fts5") {
                return Some(Module::Fts5);
            } else if eq(name, b"fts5vocab") {
                return Some(Module::Fts5Vocab);
            }
            None
        }
    }
}

impl Vtab for Module {
    fn connect(&self, name: &[u8], args: &[&[u8]], create: bool) -> Result<VTab, SchemaKind> {
        let _ = (name, create);
        let mut tab = VTab {
            module: *self,
            cols: AVec::new(),
            ndim2: 0,
            naux: 0,
            schema: AVec::new(),
            #[cfg(feature = "fts5")]
            fts: None,
            #[cfg(feature = "fts5")]
            vocab: None,
        };
        match self {
            Module::Series => {
                // `CREATE TABLE x(value,start hidden,stop hidden,step hidden)`.
                tab.cols.push(VCol::new(b"value", b"", false));
                for n in [b"start" as &[u8], b"stop", b"step"] {
                    tab.cols.push(VCol::new(n, b"", true));
                }
            }
            Module::Dbstat => {
                // statConnect: the first argument names the schema.
                if let Some(a) = args.first() {
                    let (d, _) = crate::record::dequote_q(a.trim_ascii());
                    tab.schema.extend_from_slice(d);
                }
                for (n, d) in DBSTAT_COLS.iter() {
                    tab.cols.push(VCol::new(n, d, false));
                }
                for (n, d) in [
                    (b"schema" as &[u8], b"TEXT" as &'static [u8]),
                    (b"aggregate", b"BOOLEAN"),
                ] {
                    tab.cols.push(VCol::new(n, d, true));
                }
            }
            #[cfg(feature = "rtree")]
            Module::Rtree { int } => crate::rtree::declare(&mut tab, args, *int)?,
            #[cfg(feature = "fts5")]
            Module::Fts5 => {
                let cfg = crate::fts5::config::Config::parse(name, args).map_err(|e| {
                    use crate::fts5::config::CfgErr;
                    match e {
                        CfgErr::NoTokenizer | CfgErr::Tokenizer => SchemaKind::Fts5Tokenizer,
                        _ => SchemaKind::Fts5Definition,
                    }
                })?;
                for c in cfg.cols.iter() {
                    tab.cols.push(VCol::new(c.as_slice(), b"", false));
                }
                tab.cols.push(VCol::new(name, b"", true));
                tab.cols.push(VCol::new(b"rank", b"", true));
                tab.fts = Some(alloc::boxed::Box::new(cfg));
            }
            #[cfg(feature = "fts5")]
            Module::Fts5Vocab => {
                let words: AVec<AVec<u8>> = args
                    .iter()
                    .map(|a| {
                        let a = a.trim_ascii();
                        if matches!(a.first(), Some(b'\'' | b'"' | b'`' | b'[')) {
                            crate::fts5::config::dequote(a).0
                        } else {
                            let mut v = AVec::new();
                            v.extend_from_slice(a);
                            v
                        }
                    })
                    .collect();
                let (tabname, ty) = match words.as_slice() {
                    // The three-argument form names another database's
                    // table, which only a TEMP vocab table may (refused
                    // with TEMP vocab tables).
                    [t, ty] => (t, ty),
                    _ => return Err(SchemaKind::Fts5Definition),
                };
                let ci = crate::sql::parser::eq_ci;
                let ty = ty.as_slice();
                let kind = if ci(ty, b"row") {
                    0
                } else if ci(ty, b"col") {
                    1
                } else if ci(ty, b"instance") {
                    2
                } else {
                    return Err(SchemaKind::Fts5Definition);
                };
                let names: &[&[u8]] = match kind {
                    0 => &[b"term", b"doc", b"cnt"],
                    1 => &[b"term", b"col", b"doc", b"cnt"],
                    _ => &[b"term", b"doc", b"col", b"offset"],
                };
                for n in names {
                    tab.cols.push(VCol::new(n, b"", false));
                }
                tab.vocab = Some(alloc::boxed::Box::new((tabname.clone(), kind)));
            }
            #[cfg(feature = "rtree")]
            Module::Check => {
                tab.cols.push(VCol::new(b"rtreecheck", b"", false));
                tab.cols.push(VCol::new(b"a", b"", true));
                tab.cols.push(VCol::new(b"b", b"", true));
            }
        }
        Ok(tab)
    }

    fn best_index(&self, tab: &VTab, info: &mut IndexInfo) -> Result<(), BestErr> {
        let _ = tab;
        match self {
            #[cfg(feature = "fts5")]
            Module::Fts5 => fts5_best_index(tab, info),
            #[cfg(feature = "fts5")]
            Module::Fts5Vocab => Ok(()),
            Module::Series => series_best_index(info),
            Module::Dbstat => {
                dbstat_best_index(info);
                Ok(())
            }
            #[cfg(feature = "rtree")]
            Module::Rtree { .. } => {
                crate::rtree::best_index(tab, info);
                Ok(())
            }
            #[cfg(feature = "rtree")]
            Module::Check => {
                // Both hidden columns by equality, in order.
                let mut n = 0u16;
                for want in [1i32, 2] {
                    let hit = info
                        .cons
                        .iter()
                        .rposition(|c| c.col == want && c.op == Op::Eq && c.usable);
                    if let Some(i) = hit {
                        n += 1;
                        info.set(i, n, true);
                        info.idx_num |= want;
                    }
                }
                Ok(())
            }
        }
    }

    fn eponymous(&self) -> bool {
        match self {
            Module::Series | Module::Dbstat => true,
            #[cfg(feature = "rtree")]
            Module::Check => true,
            #[cfg(feature = "rtree")]
            Module::Rtree { .. } => false,
            #[cfg(feature = "fts5")]
            Module::Fts5 | Module::Fts5Vocab => false,
        }
    }

    fn creatable(&self) -> bool {
        match self {
            Module::Dbstat => true,
            #[cfg(feature = "fts5")]
            Module::Fts5 | Module::Fts5Vocab => true,
            #[cfg(feature = "rtree")]
            Module::Rtree { .. } => true,
            _ => false,
        }
    }

    fn writable(&self) -> bool {
        #[cfg(feature = "fts5")]
        if matches!(self, Module::Fts5) {
            return true;
        }
        #[cfg(feature = "rtree")]
        if matches!(self, Module::Rtree { .. }) {
            return true;
        }
        false
    }

    fn shadow_name(&self, suffix: &[u8]) -> bool {
        #[cfg(feature = "fts5")]
        if matches!(self, Module::Fts5) {
            return [b"content" as &[u8], b"docsize", b"config", b"data", b"idx"]
                .iter()
                .any(|s| crate::sql::parser::eq_ci(s, suffix));
        }
        #[cfg(feature = "rtree")]
        if matches!(self, Module::Rtree { .. }) {
            return crate::rtree::SHADOWS
                .iter()
                .any(|s| crate::sql::parser::eq_ci(s, suffix));
        }
        let _ = suffix;
        false
    }
}

/// `fts5BestIndexMethod`: MATCH (or `=` on the hidden columns: the
/// table column is a query, `rank` the rank function), then one
/// rowid `=`, else a rowid range; idxStr lists them in order (`M`
/// with the column number, `r`, `=`, `<`, `>`). An unusable MATCH
/// makes the plan unusable. Only MATCH and the rank are omitted;
/// a trigram table also takes LIKE / GLOB patterns (`L` / `G`).
#[cfg(feature = "fts5")]
fn fts5_best_index(tab: &VTab, info: &mut IndexInfo) -> Result<(), BestErr> {
    let ncol = tab.visible() as i32;
    let mut s = AVec::new();
    let mut n = 0u16;
    let (mut seen_eq, mut seen_rank) = (false, false);
    for i in 0..info.cons.len() {
        let c = info.cons[i];
        if c.op == Op::Match || (c.op == Op::Eq && c.col >= ncol) {
            if !c.usable || c.col < 0 {
                return Err(BestErr::Unusable);
            }
            if c.col == ncol + 1 {
                if seen_rank {
                    continue;
                }
                s.push(b'r');
                seen_rank = true;
            } else {
                s.push(b'M');
                crate::record::render_int(i64::from(c.col), &mut s);
            }
            n += 1;
            info.set(i, n, true);
        } else if c.usable
            && matches!(c.op, Op::Like | Op::Glob)
            && c.col >= 0
            && c.col < ncol
            && fts5_pattern(tab).is_some_and(|glob| c.op == Op::Glob || !glob)
        {
            // fts5UsePatternMatch: a trigram table (no diacritics
            // folding) takes LIKE when case-insensitive, GLOB always;
            // the constraint is still checked.
            s.push(if c.op == Op::Like { b'L' } else { b'G' });
            crate::record::render_int(i64::from(c.col), &mut s);
            n += 1;
            info.set(i, n, false);
        } else if c.usable && !seen_eq && c.op == Op::Eq && c.col < 0 {
            s.push(b'=');
            seen_eq = true;
            n += 1;
            info.set(i, n, false);
        }
    }
    if !seen_eq {
        let (mut lt, mut gt) = (false, false);
        for i in 0..info.cons.len() {
            let c = info.cons[i];
            if c.col >= 0 || !c.usable {
                continue;
            }
            match c.op {
                Op::Lt | Op::Le if !lt => {
                    s.push(b'<');
                    lt = true;
                }
                Op::Gt | Op::Ge if !gt => {
                    s.push(b'>');
                    gt = true;
                }
                _ => continue,
            }
            n += 1;
            info.set(i, n, false);
        }
    }
    // FTS5_BI_ORDER_RANK (1) / FTS5_BI_ORDER_ROWID (2), and
    // FTS5_BI_ORDER_DESC (4).
    if let [(c, desc)] = info.order.as_slice() {
        let flag = if *c == ncol + 1 && s.iter().any(|b| matches!(b, b'M' | b'L' | b'G')) {
            1
        } else if *c == -1 {
            2
        } else {
            0
        };
        if flag != 0 {
            info.idx_num = flag | if *desc { 4 } else { 0 };
        }
    }
    // The pin's estimates (v0.17, `join_order`: the join planner's
    // costs): by the full-text and rowid constraints used.
    #[cfg(feature = "join_order")]
    {
        let n_match = s.iter().filter(|b| matches!(b, b'M' | b'L' | b'G')).count();
        let (lt, gt) = (s.iter().any(|b| *b == b'<'), s.iter().any(|b| *b == b'>'));
        if seen_eq {
            info.cost = if n_match > 0 { 25000.0 } else { 25.0 };
            info.rows = 1;
        } else {
            let mut rows: i64;
            if n_match > 0 {
                info.cost = if lt != gt { 37500.0 } else { 50000.0 };
                rows = (info.cost / 40.0) as i64;
                for _ in 1..n_match {
                    info.cost *= 2.5;
                    rows /= 2;
                }
            } else {
                info.cost = match (lt, gt) {
                    (true, true) => 750000.0,
                    (true, false) | (false, true) => 2250000.0,
                    _ => 3000000.0,
                };
                rows = (info.cost / 4.0) as i64;
            }
            info.rows = rows.max(1);
        }
    }
    info.idx_str = s;
    Ok(())
}

/// `sqlite3Fts5TokenizerPattern`: a trigram tokenizer without
/// diacritics folding matches LIKE / GLOB patterns; `Some(true)`
/// when only GLOB (case-sensitive).
#[cfg(feature = "fts5")]
fn fts5_pattern(tab: &VTab) -> Option<bool> {
    match tab.fts.as_deref()?.tok {
        crate::fts5::tok::Tokenizer::Trigram { fold, rd: 0 } => Some(!fold),
        _ => None,
    }
}

// ---------------------------------------------------------------
// generate_series
// ---------------------------------------------------------------

/// `seriesBestIndex` (the pin's ext/misc/series.c): EQ constraints
/// on start / stop / step, and value (or rowid) =, IS, >=, >, <=, <
/// — each the last of its kind wins, an equality on value overrides
/// the bounds. Every one used is omitted. A missing usable start is
/// the pin's "first argument to generate_series() missing or
/// unusable" unless a value constraint stands in for it.
fn series_best_index(info: &mut IndexInfo) -> Result<(), BestErr> {
    let mut idx_num = 0i32;
    let mut start_seen = false;
    let mut unusable = 0i32;
    let mut aidx: [Option<usize>; 7] = [None; 7];
    for (i, c) in info.cons.iter().enumerate() {
        if c.col < 1 {
            if (c.col == 0 || c.col == -1) && c.usable {
                match c.op {
                    Op::Eq | Op::Is => {
                        idx_num |= 0x80;
                        idx_num &= !0x3300;
                        aidx[5] = Some(i);
                        aidx[6] = None;
                        start_seen = true;
                    }
                    Op::Ge if idx_num & 0x80 == 0 => {
                        idx_num |= 0x100;
                        idx_num &= !0x200;
                        aidx[5] = Some(i);
                        start_seen = true;
                    }
                    Op::Gt if idx_num & 0x80 == 0 => {
                        idx_num |= 0x200;
                        idx_num &= !0x100;
                        aidx[5] = Some(i);
                        start_seen = true;
                    }
                    Op::Le if idx_num & 0x80 == 0 => {
                        idx_num |= 0x1000;
                        idx_num &= !0x2000;
                        aidx[6] = Some(i);
                    }
                    Op::Lt if idx_num & 0x80 == 0 => {
                        idx_num |= 0x2000;
                        idx_num &= !0x1000;
                        aidx[6] = Some(i);
                    }
                    _ => {}
                }
            }
            continue;
        }
        let col = c.col - 1;
        if !(0..=2).contains(&col) {
            continue;
        }
        let mask = 1i32 << col;
        if col == 0 && c.op == Op::Eq {
            start_seen = true;
        }
        if !c.usable {
            unusable |= mask;
        } else if c.op == Op::Eq {
            idx_num |= mask;
            aidx[col as usize] = Some(i);
        }
    }
    let mut n = 0u16;
    for a in aidx.iter().flatten() {
        n += 1;
        info.set(*a, n, true);
    }
    if !start_seen {
        return Err(BestErr::Err(SchemaKind::SeriesFirstArgument));
    }
    if unusable & !idx_num != 0 {
        return Err(BestErr::Unusable);
    }
    if idx_num & 0x03 == 0x03 {
        info.cost = f64::from(2 - i32::from(idx_num & 4 != 0));
        info.rows = 1000;
        if let Some((0, desc)) = info.order.as_slice().first().copied() {
            idx_num |= if desc { 0x08 } else { 0x10 };
            info.consumed = true;
        }
    } else {
        info.rows = 2_147_483_647;
    }
    info.idx_num = idx_num;
    Ok(())
}

/// A positioned `generate_series` cursor (`series_cursor`).
#[derive(Debug, Clone)]
pub struct SeriesCursor {
    obase: i64,
    oterm: i64,
    ostep: i64,
    term: i64,
    step: u64,
    desc: bool,
    value: i64,
    done: bool,
}

/// `|a - b|` as an unsigned span (`span64`).
fn span64(a: i64, b: i64) -> u64 {
    (a as u64).wrapping_sub(b as u64)
}

/// `a + b` in two's complement (`add64`).
fn add64(a: i64, b: u64) -> i64 {
    (a as u64).wrapping_add(b) as i64
}

/// `a - b` in two's complement (`sub64`).
fn sub64(a: i64, b: u64) -> i64 {
    (a as u64).wrapping_sub(b) as i64
}

/// `sqlite3_value_numeric_type(v) == SQLITE_FLOAT`: a REAL, or text
/// that reads as a real number.
fn numeric_real(v: &OwnedValue) -> bool {
    match v {
        OwnedValue::Real(_) => true,
        OwnedValue::Text(t) => matches!(
            crate::record::text_to_number(t.as_slice()),
            Some(OwnedValue::Real(_))
        ),
        _ => false,
    }
}

/// `ceil` / `floor` of a finite double (core has neither in no_std).
fn ceil_f(r: f64) -> f64 {
    let t = trunc_f(r);
    if t < r { t + 1.0 } else { t }
}

fn floor_f(r: f64) -> f64 {
    let t = trunc_f(r);
    if t > r { t - 1.0 } else { t }
}

fn trunc_f(r: f64) -> f64 {
    // Beyond 2^52 every double is integral.
    if r.is_nan() || r.abs() >= 4_503_599_627_370_496.0 {
        return r;
    }
    (r as i64) as f64
}

/// A double the pin casts to `sqlite3_int64` (C truncation; the
/// callers keep it in range).
fn d2i(r: f64) -> i64 {
    crate::query::num_to_i64(crate::query::Num::Real(r))
}

impl SeriesCursor {
    /// `seriesFilter(idxNum, argv)`.
    pub fn filter(idx_num: i32, argv: &[OwnedValue]) -> SeriesCursor {
        let none = SeriesCursor {
            obase: 0,
            oterm: 0,
            ostep: 1,
            term: 0,
            step: 1,
            desc: false,
            value: 0,
            done: true,
        };
        Self::filter_inner(idx_num, argv).unwrap_or(SeriesCursor {
            obase: 0,
            oterm: 0,
            ostep: 1,
            ..none
        })
    }

    fn filter_inner(idx_num: i32, argv: &[OwnedValue]) -> Option<SeriesCursor> {
        use crate::func::{int64_value, real_value};
        // Any NULL constraint value: no rows.
        if argv.iter().any(|v| matches!(v, OwnedValue::Null)) {
            return None;
        }
        let mut it = argv.iter();
        let mut take = || it.next().cloned().unwrap_or(OwnedValue::Null);
        let mut obase = if idx_num & 0x01 != 0 {
            int64_value(&take())
        } else {
            0
        };
        let mut oterm = if idx_num & 0x02 != 0 {
            int64_value(&take())
        } else {
            0xffff_ffff
        };
        let mut ostep = 1i64;
        if idx_num & 0x04 != 0 {
            ostep = int64_value(&take());
            if ostep == 0 {
                ostep = 1;
            } else if ostep > 0 {
                if obase > oterm {
                    return None;
                }
            } else if obase < oterm {
                return None;
            }
        }
        if idx_num & 0x05 == 0 && idx_num & 0x0380 != 0 {
            obase = i64::MIN;
        }
        if idx_num & 0x06 == 0 && idx_num & 0x3080 != 0 {
            oterm = i64::MAX;
        }
        let mut base = obase;
        let mut term = oterm;
        // The step's magnitude: 2^63 for `i64::MIN`, as the pin's
        // unsigned iStep (v0.16 review fix: it was i64::MAX).
        let step: u64 = ostep.unsigned_abs();
        let mut desc = ostep < 0;
        if !desc && base > term {
            return None;
        }
        if desc && base < term {
            return None;
        }
        let mut imin = i64::MIN;
        let mut imax = i64::MAX;
        const MIN_F: f64 = -9_223_372_036_854_775_808.0;
        const MAX_F: f64 = 9_223_372_036_854_775_807.0;
        if idx_num & 0x80 != 0 {
            let v = take();
            if numeric_real(&v) {
                let r = real_value(&v);
                if r == ceil_f(r) && (MIN_F..=MAX_F).contains(&r) {
                    imin = d2i(r);
                    imax = imin;
                } else {
                    return None;
                }
            } else {
                imin = int64_value(&v);
                imax = imin;
            }
        } else {
            if idx_num & 0x0300 != 0 {
                let v = take();
                if numeric_real(&v) {
                    let r = real_value(&v);
                    imin = if r < MIN_F {
                        i64::MIN
                    } else if idx_num & 0x200 != 0 && r == ceil_f(r) {
                        d2i(ceil_f(r + 1.0))
                    } else {
                        d2i(ceil_f(r))
                    };
                } else {
                    imin = int64_value(&v);
                    if idx_num & 0x200 != 0 {
                        imin = imin.checked_add(1)?;
                    }
                }
            }
            if idx_num & 0x3000 != 0 {
                let v = take();
                if numeric_real(&v) {
                    let r = real_value(&v);
                    imax = if r > MAX_F {
                        i64::MAX
                    } else if idx_num & 0x2000 != 0 && r == floor_f(r) {
                        d2i(r - 1.0)
                    } else {
                        d2i(floor_f(r))
                    };
                } else {
                    imax = int64_value(&v);
                    if idx_num & 0x2000 != 0 {
                        imax = imax.checked_sub(1)?;
                    }
                }
            }
            if imin > imax {
                return None;
            }
        }
        if !desc {
            if base < imin {
                let span = span64(imin, base);
                base = add64(base, (span / step).wrapping_mul(step));
                if base < imin {
                    if base > sub64(i64::MAX, step) {
                        return None;
                    }
                    base = add64(base, step);
                }
            }
            if term > imax {
                term = imax;
            }
        } else {
            if base > imax {
                let span = span64(base, imax);
                base = sub64(base, (span / step).wrapping_mul(step));
                // Still above the upper bound: one step further down
                // (v0.16 review fix: the test was `<`, so the first
                // value could exceed a consumed `value <` / `=`).
                if base > imax {
                    if base < add64(i64::MIN, step) {
                        return None;
                    }
                    base = sub64(base, step);
                }
            }
            if term < imin {
                term = imin;
            }
        }
        if !desc {
            if base > term {
                return None;
            }
            term = sub64(term, span64(term, base) % step);
        } else {
            if base < term {
                return None;
            }
            term = add64(term, span64(base, term) % step);
        }
        if (idx_num & 0x08 != 0 && !desc) || (idx_num & 0x10 != 0 && desc) {
            core::mem::swap(&mut base, &mut term);
            desc = !desc;
        }
        Some(SeriesCursor {
            obase,
            oterm,
            ostep,
            term,
            step,
            desc,
            value: base,
            done: false,
        })
    }
}

impl VtCursor for SeriesCursor {
    fn eof(&self) -> bool {
        self.done
    }

    fn next(&mut self) {
        if self.value == self.term {
            self.done = true;
        } else if self.desc {
            self.value = sub64(self.value, self.step);
        } else {
            self.value = add64(self.value, self.step);
        }
    }

    fn column(&self, i: usize) -> OwnedValue {
        OwnedValue::Integer(match i {
            1 => self.obase,
            2 => self.oterm,
            3 => self.ostep,
            _ => self.value,
        })
    }

    fn rowid(&self) -> i64 {
        self.value
    }
}

/// A cursor over rows a module computed up front.
#[derive(Debug, Clone, Default)]
pub struct RowsCursor {
    pub rows: AVec<(AVec<OwnedValue>, i64)>,
    pub pos: usize,
}

impl VtCursor for RowsCursor {
    fn eof(&self) -> bool {
        self.pos >= self.rows.len()
    }

    fn next(&mut self) {
        self.pos = self.pos.saturating_add(1);
    }

    fn column(&self, i: usize) -> OwnedValue {
        self.rows
            .as_slice()
            .get(self.pos)
            .and_then(|r| r.0.as_slice().get(i))
            .cloned()
            .unwrap_or(OwnedValue::Null)
    }

    fn rowid(&self) -> i64 {
        self.rows.as_slice().get(self.pos).map_or(0, |r| r.1)
    }
}

/// A cursor's rows: each row's columns and its rowid.
pub type CursorRows = AVec<(AVec<OwnedValue>, Option<i64>)>;

/// Read a cursor to its end (at most `need` rows): each row's
/// `ncols` columns and its rowid. More than `max` rows (v0.16 review
/// fix: `Config::max_vtab_rows`), or a refused allocation, is
/// `VtabTooManyRows` — the materialising paths hold the rows (a
/// streamed query reads the cursor row by row instead, v0.17), and
/// `generate_series(1)` alone is 2^32 of them.
pub fn drain(
    cur: &mut dyn VtCursor,
    ncols: usize,
    need: Option<usize>,
    max: usize,
) -> Result<CursorRows, SchemaKind> {
    let mut out = AVec::new();
    while !cur.eof() && need.is_none_or(|n| out.len() < n) {
        if out.len() >= max || out.try_reserve(1).is_err() {
            return Err(SchemaKind::VtabTooManyRows);
        }
        let mut row = AVec::new();
        for i in 0..ncols {
            row.push(cur.column(i));
        }
        out.push((row, Some(cur.rowid())));
        cur.next();
    }
    Ok(out)
}

// ---------------------------------------------------------------
// dbstat
// ---------------------------------------------------------------

const DBSTAT_COLS: [(&[u8], &[u8]); 10] = [
    (b"name", b"TEXT"),
    (b"path", b"TEXT"),
    (b"pageno", b"INTEGER"),
    (b"pagetype", b"TEXT"),
    (b"ncell", b"INTEGER"),
    (b"payload", b"INTEGER"),
    (b"unused", b"INTEGER"),
    (b"mx_payload", b"INTEGER"),
    (b"pgoffset", b"INTEGER"),
    (b"pgsize", b"INTEGER"),
];

/// `statBestIndex`: EQ on name (0), schema (10), aggregate (11);
/// schema is omitted; rows come in (name, path) order when asked.
fn dbstat_best_index(info: &mut IndexInfo) {
    let (mut sch, mut name, mut agg) = (None, None, None);
    for (i, c) in info.cons.iter().enumerate() {
        if c.op != Op::Eq || !c.usable {
            continue;
        }
        match c.col {
            0 => name = Some(i),
            10 => sch = Some(i),
            11 => agg = Some(i),
            _ => {}
        }
    }
    let mut n = 0u16;
    if let Some(i) = sch {
        n += 1;
        info.set(i, n, true);
        info.idx_num |= 1;
    }
    if let Some(i) = name {
        n += 1;
        info.set(i, n, false);
        info.idx_num |= 2;
    }
    if let Some(i) = agg {
        n += 1;
        info.set(i, n, false);
        info.idx_num |= 4;
    }
    info.cost = 1.0;
    let o = info.order.as_slice();
    if matches!(o, [(0, false)] | [(0, false), (1, false)]) {
        info.consumed = true;
        info.idx_num |= 8;
    }
}

/// What dbstat's filter read from its arguments.
#[derive(Debug, Clone)]
pub struct StatArgs {
    /// The schema to walk (`schema = ?`, else the table's).
    pub schema: Option<AVec<u8>>,
    /// `name = ?` (its text; NULL passes no filter, as on the pin).
    pub name: Option<AVec<u8>>,
    pub agg: bool,
    /// Rows in name order (ORDER BY name consumed).
    pub ordered: bool,
}

/// `statFilter`'s argument decoding.
pub fn stat_args(idx_num: i32, argv: &[OwnedValue]) -> StatArgs {
    let mut it = argv.iter();
    let text = |v: Option<&OwnedValue>| -> Option<AVec<u8>> {
        let mut t = AVec::new();
        match v? {
            OwnedValue::Null => return None,
            OwnedValue::Text(b) | OwnedValue::Blob(b) => {
                let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
                t.extend_from_slice(b.as_slice().get(..end).unwrap_or(&[]));
            }
            OwnedValue::Integer(i) => crate::record::render_int(*i, &mut t),
            OwnedValue::Real(r) => crate::record::render_real(*r, &mut t),
        }
        Some(t)
    };
    let mut out = StatArgs {
        schema: None,
        name: None,
        agg: false,
        ordered: idx_num & 8 != 0,
    };
    if idx_num & 1 != 0 {
        // A NULL schema finds no database (no rows).
        out.schema = Some(text(it.next()).unwrap_or_default());
    }
    if idx_num & 2 != 0 {
        out.name = text(it.next());
    }
    if idx_num & 4 != 0 {
        out.agg = it.next().is_some_and(|v| crate::func::real_value(v) != 0.0);
    }
    out
}

/// One b-tree page as `statDecodePage` reads it.
struct StatPage {
    pgno: u32,
    flags: u8,
    ncell: usize,
    unused: i64,
    mx_payload: i64,
    right: u32,
    /// (child page, local payload, overflow pages, last overflow
    /// page's payload).
    cells: AVec<(u32, i64, AVec<u32>, i64)>,
    path: AVec<u8>,
    icell: usize,
}

/// The counters dbstat reports for a row (`StatCursor`).
#[derive(Default)]
struct StatAcc {
    npage: i64,
    ncell: i64,
    payload: i64,
    unused: i64,
    mx_payload: i64,
    pgsize: i64,
}

/// The pin's `getLocalPayload`.
fn local_payload(usable: i64, flags: u8, payload: i64) -> i64 {
    let (min_local, max_local) = if flags == 0x0D {
        ((usable - 12) * 32 / 255 - 23, usable - 35)
    } else {
        ((usable - 12) * 32 / 255 - 23, (usable - 12) * 64 / 255 - 23)
    };
    let mut n = min_local + (payload - min_local) % (usable - 4);
    if n > max_local {
        n = min_local;
    }
    if payload > max_local { n } else { payload }
}

fn be16(d: &[u8], at: usize) -> usize {
    match d.get(at..at + 2) {
        Some(&[a, b]) => usize::from(a) << 8 | usize::from(b),
        _ => 0,
    }
}

fn be32(d: &[u8], at: usize) -> u32 {
    match d.get(at..at + 4) {
        Some(&[a, b, c, e]) => u32::from_be_bytes([a, b, c, e]),
        _ => 0,
    }
}

/// `statDecodePage` (a page it cannot read is "corrupted": flags 0,
/// no cells).
async fn stat_decode<S: crate::storage::Storage>(
    pager: &mut crate::pager::Pager<S>,
    pgno: u32,
    path: AVec<u8>,
) -> crate::error::Result<StatPage, S::Error> {
    let mut p = StatPage {
        pgno,
        flags: 0,
        ncell: 0,
        unused: 0,
        mx_payload: 0,
        right: 0,
        cells: AVec::new(),
        path,
        icell: 0,
    };
    if pgno == 0 || pgno > pager.page_count() {
        return Ok(p);
    }
    let sz = i64::from(pager.page_size());
    let usable = i64::from(pager.usable());
    let idx = pager.load(pgno).await?;
    let mut data = AVec::new();
    data.extend_from_slice(pager.frame_data(idx));
    let d = data.as_slice();
    let hoff = if pgno == 1 { 100 } else { 0 };
    let flags = d.get(hoff).copied().unwrap_or(0);
    let (leaf, mut nhdr) = match flags {
        0x0A | 0x0D => (true, 8usize),
        0x02 | 0x05 => (false, 12usize),
        _ => return Ok(p),
    };
    nhdr += hoff;
    let ncell = be16(d, hoff + 3);
    let mut unused = be16(d, hoff + 5) as i64 - nhdr as i64 - 2 * ncell as i64;
    unused += i64::from(d.get(hoff + 7).copied().unwrap_or(0));
    let mut off = be16(d, hoff + 1);
    let mut guard = 0usize;
    while off != 0 {
        if off as i64 >= sz || guard > d.len() {
            return Ok(p);
        }
        unused += be16(d, off + 2) as i64;
        let next = be16(d, off);
        if next < off + 4 && next > 0 {
            return Ok(p);
        }
        off = next;
        guard += 1;
    }
    p.flags = flags;
    p.ncell = ncell;
    p.unused = unused;
    p.right = if leaf { 0 } else { be32(d, hoff + 8) };
    for i in 0..ncell {
        let mut at = be16(d, nhdr + 2 * i);
        if at < nhdr || at as i64 >= sz {
            p.flags = 0;
            p.cells.clear();
            return Ok(p);
        }
        let mut child = 0u32;
        if !leaf {
            child = be32(d, at);
            at += 4;
        }
        if flags == 0x05 {
            p.cells.push((child, 0, AVec::new(), 0));
            continue;
        }
        let Some((payload, n)) = d
            .get(at..)
            .and_then(|b| crate::record::decode_varint::<()>(b).ok())
        else {
            p.flags = 0;
            p.cells.clear();
            return Ok(p);
        };
        at += n;
        if flags == 0x0D {
            let Some((_, n)) = d
                .get(at..)
                .and_then(|b| crate::record::decode_varint::<()>(b).ok())
            else {
                p.flags = 0;
                p.cells.clear();
                return Ok(p);
            };
            at += n;
        }
        let payload = payload as i64;
        if payload > p.mx_payload {
            p.mx_payload = payload;
        }
        let local = local_payload(usable, flags, payload);
        let mut ovfl = AVec::new();
        let mut last = 0i64;
        if payload > local {
            let nov = (payload - local + usable - 5) / (usable - 4);
            if at as i64 + local + 4 > usable || payload > 0x7fff_ffff {
                p.flags = 0;
                p.cells.clear();
                return Ok(p);
            }
            last = (payload - local) - (nov - 1) * (usable - 4);
            let mut pg = be32(d, at + local as usize);
            ovfl.push(pg);
            for _ in 1..nov {
                if pg == 0 || pg > pager.page_count() {
                    break;
                }
                let j = pager.load(pg).await?;
                pg = be32(pager.frame_data(j), 0);
                ovfl.push(pg);
            }
        }
        p.cells.push((child, local, ovfl, last));
    }
    Ok(p)
}

fn hex_into(out: &mut AVec<u8>, v: usize, width: usize) {
    let mut digits = [0u8; 16];
    let mut n = 0usize;
    let mut x = v;
    loop {
        if let Some(d) = digits.get_mut(n) {
            *d = b"0123456789abcdef".get(x & 15).copied().unwrap_or(b'0');
        }
        n += 1;
        x >>= 4;
        if x == 0 || n >= 16 {
            break;
        }
    }
    for _ in n..width {
        out.push(b'0');
    }
    for i in (0..n).rev() {
        out.push(digits.get(i).copied().unwrap_or(b'0'));
    }
}

/// One dbstat output row (`statColumn` 0..=11) for btree `name`.
#[allow(clippy::too_many_arguments)]
fn stat_row(
    name: &[u8],
    schema: &[u8],
    agg: bool,
    path: Option<&[u8]>,
    pageno: i64,
    pagetype: Option<&[u8]>,
    acc: &StatAcc,
    offset: i64,
) -> AVec<OwnedValue> {
    let text = |b: &[u8]| {
        let mut t = AVec::new();
        t.extend_from_slice(b);
        OwnedValue::Text(t)
    };
    let mut r = AVec::new();
    r.push(text(name));
    r.push(path.map_or(OwnedValue::Null, text));
    r.push(OwnedValue::Integer(if agg { acc.npage } else { pageno }));
    r.push(pagetype.map_or(OwnedValue::Null, text));
    r.push(OwnedValue::Integer(acc.ncell));
    r.push(OwnedValue::Integer(acc.payload));
    r.push(OwnedValue::Integer(acc.unused));
    r.push(OwnedValue::Integer(acc.mx_payload));
    r.push(if agg {
        OwnedValue::Null
    } else {
        OwnedValue::Integer(offset)
    });
    r.push(OwnedValue::Integer(acc.pgsize));
    r.push(text(schema));
    r.push(OwnedValue::Integer(i64::from(agg)));
    r
}

/// dbstat's rows for the b-trees `trees` ((name, root) in the order
/// the module reads them): `statNext`'s walk — each page, then its
/// cells' overflow pages, then (interior pages) each child's subtree
/// in cell order and the right child's; with `agg` one row per tree.
/// At most `need` rows.
pub async fn dbstat_rows<S: crate::storage::Storage>(
    pager: &mut crate::pager::Pager<S>,
    trees: &[(AVec<u8>, u32)],
    schema: &[u8],
    agg: bool,
    need: Option<usize>,
) -> crate::error::Result<AVec<(AVec<OwnedValue>, Option<i64>)>, S::Error> {
    let mut out: AVec<(AVec<OwnedValue>, Option<i64>)> = AVec::new();
    if pager.page_count() == 0 {
        return Ok(out);
    }
    let psz = i64::from(pager.page_size());
    let usable = i64::from(pager.usable());
    let full = |out: &AVec<(AVec<OwnedValue>, Option<i64>)>| need.is_some_and(|n| out.len() >= n);
    for (name, root) in trees.iter() {
        if full(&out) {
            break;
        }
        let mut acc = StatAcc::default();
        let mut path = AVec::new();
        path.push(b'/');
        let mut stack: AVec<StatPage> = AVec::new();
        let mut last_pg;
        // A page's own row, as `statNext` reports it when it first
        // reaches the page.
        let emit_page =
            |p: &StatPage, acc: &mut StatAcc, out: &mut AVec<(AVec<OwnedValue>, Option<i64>)>| {
                if !agg {
                    *acc = StatAcc::default();
                }
                acc.npage += 1;
                acc.pgsize += psz;
                acc.ncell += p.ncell as i64;
                acc.unused += p.unused;
                if p.mx_payload > acc.mx_payload {
                    acc.mx_payload = p.mx_payload;
                }
                acc.payload += p.cells.iter().map(|c| c.1).sum::<i64>();
                if !agg {
                    let ty: &[u8] = match p.flags {
                        0x05 | 0x02 => b"internal",
                        0x0D | 0x0A => b"leaf",
                        _ => b"corrupted",
                    };
                    let pg = i64::from(p.pgno);
                    let row = stat_row(
                        name.as_slice(),
                        schema,
                        false,
                        Some(p.path.as_slice()),
                        pg,
                        Some(ty),
                        acc,
                        psz * (pg - 1),
                    );
                    out.push((row, Some(pg)));
                }
            };
        let first = stat_decode(pager, *root, path).await?;
        emit_page(&first, &mut acc, &mut out);
        last_pg = i64::from(first.pgno);
        stack.push(first);
        while let Some(top) = stack.as_mut_slice().last_mut() {
            if !agg && full(&out) {
                break;
            }
            // This cell's overflow pages.
            if let Some(cell) = top.cells.as_slice().get(top.icell) {
                let (child, _, ovfl, lastn) = (cell.0, cell.1, cell.2.clone(), cell.3);
                let ci = top.icell;
                for (k, pg) in ovfl.iter().enumerate() {
                    if !agg {
                        acc = StatAcc::default();
                    }
                    acc.npage += 1;
                    acc.pgsize += psz;
                    if k + 1 < ovfl.len() {
                        acc.payload += usable - 4;
                    } else {
                        acc.payload += lastn;
                        acc.unused += usable - 4 - lastn;
                    }
                    if !agg {
                        let mut pth = top.path.clone();
                        hex_into(&mut pth, ci, 3);
                        pth.push(b'+');
                        hex_into(&mut pth, k, 6);
                        let pgi = i64::from(*pg);
                        let row = stat_row(
                            name.as_slice(),
                            schema,
                            false,
                            Some(pth.as_slice()),
                            pgi,
                            Some(b"overflow"),
                            &acc,
                            psz * (pgi - 1),
                        );
                        out.push((row, Some(pgi)));
                    }
                }
                if top.right == 0 {
                    top.icell += 1;
                    continue;
                }
                // Descend into this cell's child.
                let mut pth = top.path.clone();
                hex_into(&mut pth, ci, 3);
                pth.push(b'/');
                top.icell += 1;
                if stack.len() >= 64 {
                    break;
                }
                let p = stat_decode(pager, child, pth).await?;
                emit_page(&p, &mut acc, &mut out);
                last_pg = i64::from(p.pgno);
                stack.push(p);
                continue;
            }
            if top.right != 0 && top.icell == top.ncell {
                let mut pth = top.path.clone();
                hex_into(&mut pth, top.icell, 3);
                pth.push(b'/');
                let child = top.right;
                top.icell += 1;
                if stack.len() >= 64 {
                    break;
                }
                let p = stat_decode(pager, child, pth).await?;
                emit_page(&p, &mut acc, &mut out);
                last_pg = i64::from(p.pgno);
                stack.push(p);
                continue;
            }
            stack.pop();
        }
        if agg {
            let row = stat_row(name.as_slice(), schema, true, None, 0, None, &acc, 0);
            out.push((row, Some(last_pg)));
        }
    }
    if let Some(n) = need {
        out.truncate(n);
    }
    Ok(out)
}
