//! v0.17 feature names (ADR 0006): the Cargo features are named for
//! what they enable, and their implications hold in every build —
//! `sql` implies `write`, which implies `query`; `planner` implies
//! `sql` and `expr_index`; every heavyweight leaf implies `sql`
//! (`math` implies only `query`). The profiles carry the features
//! their ADRs give them. Meaningful under any feature set; `cargo
//! test` runs it with the default `std,xl`.

/// `on` implies `needs`, or the build's feature graph is wrong.
fn implies(on: bool, needs: bool, what: &str) {
    assert!(!on || needs, "{what}");
}

#[test]
fn dialect_features_imply_each_other() {
    implies(
        cfg!(feature = "sql"),
        cfg!(feature = "write"),
        "`sql` implies `write`",
    );
    implies(
        cfg!(feature = "write"),
        cfg!(feature = "query"),
        "`write` implies `query`",
    );
    implies(
        cfg!(feature = "math"),
        cfg!(feature = "query"),
        "`math` implies `query`",
    );
}

#[test]
fn leaves_imply_sql() {
    let leaves = [
        ("window", cfg!(feature = "window")),
        ("triggers", cfg!(feature = "triggers")),
        ("foreign_keys", cfg!(feature = "foreign_keys")),
        ("json", cfg!(feature = "json")),
        ("expr_index", cfg!(feature = "expr_index")),
        ("without_rowid", cfg!(feature = "without_rowid")),
        ("utf16", cfg!(feature = "utf16")),
        ("wal", cfg!(feature = "wal")),
        ("datetime", cfg!(feature = "datetime")),
        ("attach", cfg!(feature = "attach")),
        ("autovacuum", cfg!(feature = "autovacuum")),
        ("vtab", cfg!(feature = "vtab")),
        ("rtree", cfg!(feature = "rtree")),
        ("fts5", cfg!(feature = "fts5")),
        ("fts5_delete", cfg!(feature = "fts5_delete")),
        ("explain", cfg!(feature = "explain")),
        ("planner", cfg!(feature = "planner")),
    ];
    for (name, on) in leaves {
        implies(on, cfg!(feature = "sql"), name);
    }
    let planner = cfg!(feature = "planner");
    implies(
        planner,
        cfg!(feature = "expr_index"),
        "`planner` implies `expr_index`",
    );
    implies(
        planner,
        cfg!(feature = "indexes"),
        "`expr_index` implies `indexes`",
    );
}

#[test]
fn profiles_carry_their_features() {
    implies(
        cfg!(feature = "full"),
        cfg!(all(
            feature = "indexes",
            feature = "query",
            feature = "write",
            feature = "sql",
            feature = "params",
            feature = "math"
        )),
        "`full`",
    );
    implies(
        cfg!(feature = "max"),
        cfg!(all(
            feature = "full",
            feature = "window",
            feature = "triggers",
            feature = "foreign_keys",
            feature = "json",
            feature = "expr_index",
            feature = "without_rowid",
            feature = "utf16",
            feature = "wal",
            feature = "datetime",
            feature = "attach",
            feature = "autovacuum",
            feature = "vtab",
            feature = "rtree",
            feature = "fts5",
            feature = "explain"
        )),
        "`max`",
    );
    implies(
        cfg!(feature = "xl"),
        cfg!(all(
            feature = "max",
            feature = "fts5_delete",
            feature = "planner"
        )),
        "`xl`",
    );
}

/// The default build (the CLI's, the differential suites') is `xl`
/// with `std`: the planner and the whole dialect are on.
#[test]
fn default_build_is_xl() {
    let default = cfg!(all(feature = "std", feature = "xl"));
    implies(default, cfg!(feature = "planner"), "`xl` has `planner`");
    implies(default, cfg!(feature = "sql"), "`xl` has `sql`");
}
