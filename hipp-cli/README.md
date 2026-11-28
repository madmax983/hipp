# hipp-cli

`hipp` is a command-line shell for [hipp](../README.md) that behaves like
SQLite's `sqlite3` shell (pinned to 3.54.1): the same command line, output
modes, dot-commands and error messages. Its output can be diffed byte for
byte against the real shell's, and its main job is the differential test
suite (`hipp-cli/tests/`). It also works for exploring a database.

It is a separate `std` crate in the workspace, built on the default (`xl`)
profile. It adds 0 bytes to the core library's builds.

## Build and run

```bash
cargo run -p hipp-cli -- [OPTIONS] [FILENAME [SQL...]]
```

```bash
cargo build -p hipp-cli --release
```

The release binary is `target/release/hipp`. With no FILENAME the database
is `:memory:`. A named file is created when something is first written to
it.

## Modes of use

**One-shot.** Each SQL argument is run in order:

```text
$ hipp demo.db "CREATE TABLE t(a, b)" "INSERT INTO t VALUES (1, 'x'), (2, NULL)"
$ hipp -box demo.db "SELECT * FROM t"
╭───┬───╮
│ a │ b │
╞═══╪═══╡
│ 1 │ x │
│ 2 │   │
╰───┴───╯
```

**Batch.** Statements and dot-commands on stdin:

```text
$ printf '.mode json\nSELECT * FROM t;\n' | hipp demo.db
[{"a":1,"b":"x"},
{"a":2,"b":null}]
```

**Interactive.** Run `hipp demo.db` in a terminal. There is no line editing.

## Options

These are taken, as the `sqlite3` shell takes them (each also spelled `--x`):

| option | |
|---|---|
| `-help`, `-version` | usage, and `3.54.1 ... hipp-0.16.0` |
| `-list`, `-csv`, `-column`, `-line`, `-box`, `-markdown`, `-table`, `-quote`, `-json`, `-tabs`, `-ascii` | the output mode |
| `-header`, `-noheader` | column headers on or off |
| `-separator SEP`, `-newline SEP`, `-nullvalue TEXT` | output formatting |
| `-cmd COMMAND` | run COMMAND before reading stdin |
| `-bail` | stop at the first error |
| `-echo` | print each input before running it |
| `-batch` | force batch I/O |
| `-noinit` | do not read `~/.sqliterc` |
| `--`, `-` | end of options |

`-help` prints the `sqlite3` shell's full option list. The options not in the
table above (`-readonly`, `-vfs`, `-deserialize`, `-safe`, ...) are refused
by name:

```text
$ hipp -readonly demo.db "SELECT 1"
hipp: Error: option not supported by hipp: -readonly
```

## Dot-commands

| command | |
|---|---|
| `.tables [PATTERN]`, `.indexes [TABLE]` | list tables / indexes |
| `.schema [--indent] [--nosys] [PATTERN]`, `.fullschema` | show the schema (LIKE / GLOB patterns) |
| `.dump [--data-only] [--nosys] [--preserve-rowids] [--newlines] [TABLE]` | SQL text that recreates the database |
| `.databases` | the main, temp and attached databases |
| `.open FILE` | open another database |
| `.read FILE` | run a script |
| `.mode MODE` | `list`, `csv`, `column`, `line`, `box`, `markdown`, `table`, `quote`, `json`, `tabs`, `insert`, `ascii` |
| `.headers on\|off`, `.separator`, `.nullvalue`, `.width` | formatting |
| `.output [FILE]`, `.once FILE` | send output to a file |
| `.parameter set\|unset\|clear\|list\|init` | bound parameters, bound by name as the `sqlite3` shell binds them |
| `.print`, `.echo`, `.bail`, `.changes`, `.timer`, `.limit`, `.show` | the shell's settings and helpers |
| `.help [PATTERN]` | help text |
| `.exit [N]`, `.quit` | leave |

`.import` is not implemented, and nor are the shell's other commands. They
are refused by name.

## Parameters and query plans

```text
$ printf "EXPLAIN QUERY PLAN SELECT * FROM t WHERE rowid=1;\n.parameter set :x 2\nSELECT * FROM t WHERE a = :x;\n" | hipp demo.db
QUERY PLAN
`--SEARCH t USING INTEGER PRIMARY KEY (rowid=?)
2|
```

## Differential tests

`hipp-cli/tests/cli_*.rs` run the same input through `hipp` and the pinned
`sqlite3` and compare stdout, stderr and exit codes. They need the oracle:
build it with `ci/build-oracle.sh`, or set `HIPP_ORACLE` to a 3.54.1
`sqlite3` binary. See [`CONTRIBUTING.md`](../CONTRIBUTING.md).

```bash
cargo test -p hipp-cli
```
