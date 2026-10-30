//! v0.12 real-literal parsing exactness, differentially vs the
//! pinned oracle (SQLite 3.54.1, RISKS R5).
//!
//! Before v0.12 the lexer's `parse_real` accumulated at most 17
//! significant digits into a `u64` and scaled by repeated
//! multiplication/division with powers of ten in chunks of 1e16.
//! Probing on the pin before implementation showed that is not a
//! rare 1-ulp effect: on an 869-literal adversarial corpus the
//! old parser's bits diverged from the correctly rounded value
//! on 386 literals (e.g. `1e308` was off by ~41 ulp,
//! `0.9999999999999999` parsed to exactly 1.0, the max-finite
//! literal `1.7976931348623157e308` parsed one ulp low, and the
//! subnormal midpoint `2.4703282292062328e-324` parsed to 0).
//!
//! Deeper probing found the pin's own conversion is not
//! textbook IEEE rounding on long inputs, and v0.12 reproduces
//! the pin's actual algorithm (see `sql::lexer::parse_real`):
//! significant digits accumulate into a `u64` and the first
//! digit that would overflow it ends the input (later digits
//! are ignored entirely, one exponent unit per dropped digit);
//! the truncated decimal is then converted exactly,
//! ties-to-even. That is why the 20-digit integer
//! `18446744073709553665` converts as `...53660` (to 2^64, not
//! 2^64+4096), why sig digit 21 of the 1.0 midpoint is ignored,
//! and why long-digit exact ties land on the lower neighbour.
//! Exactness here means: hipp's bits equal the oracle's bits,
//! for literals, for `CAST`/affinity text conversions, and for
//! the REAL values stored into and compared in database files.
//!
//! Oracle bits are extracted losslessly: the oracle writes the
//! literal into a REAL column of a file, and hipp reads the
//! file back (the record codec stores REAL as the 8-byte
//! big-endian f64, so the bits hipp reads are the bits the
//! oracle parsed). Hipp's own parse is read via `SELECT <lit>`
//! over a memory database. The reverse direction extracts
//! `printf('%!.25e', v)` from the oracle for both an
//! oracle-written and a hipp-written file and compares the
//! strings.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, MemStorage, Step};

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str)
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    assert_eq!(st.step().await.expect("step"), Step::Done);
}

/// The f64 bits of `SELECT <sql>` evaluated by hipp in memory.
fn hipp_select_bits(sql: &str) -> u64 {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::default())
            .await
            .expect("open");
        let mut q = db.prepare(sql).await.expect("prepare");
        assert_eq!(q.step().await.expect("step"), Step::Row);
        let f = q.column_f64(0).expect("numeric column");
        f.to_bits()
    })
}

/// Build the literal corpus: hand-picked edges plus a
/// deterministic LCG-generated spread of mantissa lengths and
/// exponents. Every entry is SQL literal text (a leading `-`
/// is part of the entry where present).
fn corpus() -> Vec<String> {
    let mut lits: Vec<String> = [
        // Everyday values the old parser mostly got right.
        "0.1",
        "0.2",
        "0.3",
        "1.1",
        "2.675",
        "3.14159265358979",
        "1.5",
        "2.5",
        "0.5",
        "100.0",
        "1e2",
        "1.5e-7",
        "123.456",
        "0.000001",
        // Long / pathological mantissas.
        "0.30000000000000004",
        "3.141592653589793238462643383279",
        "0.9999999999999999",
        "1.0000000000000002",
        "123456789.123456789",
        "123456789012345678901234567890.5",
        "0.17976931348623157e309",
        "9999999999999999999999999999999999999999.9999",
        "11111111111111111111111111111111111111111111111111",
        "99999999999999999999999999999999999999999999999999e-25",
        // Exponent extremes and the top of the range.
        "1e308",
        "9.999999999999999e307",
        "1e23",
        "1e22",
        "1.7976931348623157e308",
        "1.7976931348623158e308",
        "1.7976931348623159e308",
        "1e309",
        "1e999",
        "1.5e310",
        // Bottom of the range: normals/subnormals boundary.
        "2.2250738585072014e-308",
        "2.2250738585072009e-308",
        "2.225073858507201e-308",
        "1e-320",
        "1e-323",
        "5e-324",
        "4.9e-324",
        "2.5e-324",
        "2.47e-324",
        "2.4703282292062327208e-324",
        "2.4703282292062327209e-324",
        "7.410984687618698e-324",
        "1e-324",
        "1e-330",
        "1e-999",
        // Integer literals that overflow i64 become reals.
        "9223372036854775807",
        "9223372036854775808",
        "18446744073709551615",
        "18446744073709551616",
        "18446744073709551617",
        "18446744073709553664",
        "18446744073709553665",
        "1000000000000000000000000000000",
        "9007199254740993.0",
        "9007199254740992.0",
        "12345678901234567890123456789012345678901234567890",
        // Halfway cases (exact decimal expansions; ties go to
        // the even mantissa on the pin).
        "1.00000000000000011102230246251565404236316680908203125",
        "1.000000000000000111022302462515654042363166809082031250000001",
        "1.000000000000000111022302462515654042363166809082031249999999",
        "1.00000000000000033306690738754696212708950042724609375",
        "0.500000000000000055511151231257827021181583404541015625",
        "0.999999999999999944488848768742172978818416595458984375",
        // Forms: leading dot, trailing dot, signed exponent.
        ".5",
        ".25e2",
        "5.",
        "5.e3",
        "0.5e+3",
        "007.5",
        "0.0",
        "0e999",
        "0.0e-999",
        // Negatives (stored/parsed via unary minus).
        "-0.1",
        "-1.0000000000000002",
        "-1e308",
        "-4.9e-324",
        "-9223372036854775809",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    // Deterministic LCG corpus: mantissas of 1..80 digits at
    // decimal exponents across the whole f64 range, in plain,
    // dotted and exponent forms.
    let mut state: u64 = 0x243f6a8885a308d3;
    let mut rnd = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    for _ in 0..420 {
        let nd = 1 + (rnd() % 80) as usize;
        let mut digits = String::new();
        for _ in 0..nd {
            digits.push(char::from(b'0' + (rnd() % 10) as u8));
        }
        if digits.trim_start_matches('0').is_empty() {
            digits.push('7');
        }
        let exp = (rnd() % 641) as i64 - 330;
        match rnd() % 4 {
            0 => lits.push(format!("{digits}e{exp}")),
            1 => {
                let first = &digits[..1];
                let rest = &digits[1.min(digits.len())..];
                lits.push(format!("{first}.{rest}e{exp}"));
            }
            2 => lits.push(format!("0.{digits}e{}", exp + nd as i64)),
            _ => lits.push(format!("{digits}.0")),
        }
    }
    lits
}

/// Oracle bits for every corpus literal, batched: the oracle
/// inserts each literal into a REAL column; hipp reads the
/// file and returns the stored bits in rowid order.
fn oracle_bits(lits: &[String], tag: &str) -> Vec<u64> {
    let path = common::temp_db(tag);
    let values = lits
        .iter()
        .map(|l| format!("({l})"))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!("CREATE TABLE r (v REAL); INSERT INTO r VALUES {values};");
    let out = common::cli(&path, &script);
    assert_eq!(out, "", "oracle insert failed for {tag}");
    block_on(async {
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut q = db
            .prepare("SELECT v FROM r ORDER BY rowid")
            .await
            .expect("prep");
        let mut bits = Vec::new();
        while q.step().await.expect("step") == Step::Row {
            bits.push(q.column_f64(0).expect("real column").to_bits());
        }
        bits
    })
}

#[test]
fn literal_bits_match_oracle_on_corpus() {
    if !common::oracle_is_pinned() {
        panic!("oracle is not the 3.54.1 pin: {}", common::oracle_version());
    }
    let lits = corpus();
    let want = oracle_bits(&lits, "v12-corpus");
    assert_eq!(want.len(), lits.len());
    let mut mismatches = Vec::new();
    for (lit, w) in lits.iter().zip(want.iter()) {
        let got = hipp_select_bits(&format!("SELECT {lit}"));
        if got != *w {
            mismatches.push(format!("{lit}: hipp {got:#018x} != oracle {w:#018x}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} literals diverge:\n{}",
        mismatches.len(),
        lits.len(),
        mismatches.join("\n")
    );
}

#[test]
fn hipp_written_reals_extract_identically_in_oracle() {
    // Same literals, reverse file direction: hipp writes the
    // file, the oracle extracts printf('%!.25e') strings from
    // both files — the strings must be byte-identical.
    let lits = corpus();
    let oracle_path = common::temp_db("v12-fwd-oracle");
    let values = lits
        .iter()
        .map(|l| format!("({l})"))
        .collect::<Vec<_>>()
        .join(",");
    common::cli(
        &oracle_path,
        &format!("CREATE TABLE r (v REAL); INSERT INTO r VALUES {values};"),
    );
    let hipp_path = common::temp_db("v12-fwd-hipp");
    block_on(async {
        let fs = FileStorage::open(&hipp_path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        exec(&mut db, "CREATE TABLE r (v REAL)").await;
        exec(&mut db, &format!("INSERT INTO r VALUES {values}")).await;
    });
    assert_eq!(
        common::cli(&hipp_path, "PRAGMA integrity_check;").trim(),
        "ok"
    );
    let q = "SELECT printf('%!.25e', v) FROM r ORDER BY rowid;";
    let a = common::cli(&oracle_path, q);
    let b = common::cli(&hipp_path, q);
    assert_eq!(a, b, "oracle extraction differs between the files");
}

/// Exact-tie and boundary cases with the pin's bit patterns:
/// generated from the exact decimal expansions of the
/// midpoints themselves, converted under the pin's probed
/// u64 digit-horizon rule, and cross-checked against the
/// oracle's `printf('%!.25e')` identification of each value.
/// The oracle corpus above covers these literals too; this
/// table pins them without the oracle in the loop.
const SPECIAL: &[(&str, u64)] = &[
    (
        "1.00000000000000011102230246251565404236316680908203125",
        0x3ff0000000000000,
    ),
    (
        "1.00000000000000033306690738754696212708950042724609375",
        0x3ff0000000000001,
    ),
    (
        "0.500000000000000055511151231257827021181583404541015625",
        0x3fe0000000000000,
    ),
    (
        "0.999999999999999944488848768742172978818416595458984375",
        0x3fefffffffffffff,
    ),
    (
        "1.000000000000000111022302462515654042363166809082031250000001",
        0x3ff0000000000000,
    ),
    (
        "1.000000000000000111022302462515654042363166809082031249999999",
        0x3ff0000000000000,
    ),
    (
        "1.0000000000000002220446049250313080847263336181640625",
        0x3ff0000000000001,
    ),
    (
        "0.99999999999999988897769753748434595763683319091796875",
        0x3fefffffffffffff,
    ),
    ("0.1", 0x3fb999999999999a),
    ("1e308", 0x7fe1ccf385ebc8a0),
    ("0.9999999999999999", 0x3fefffffffffffff),
    ("123456789.123456789", 0x419d6f34547e6b75),
    (
        "1.79769313486231570814527423731704356798070567525844996598917476803157260780028538760589558632766878171540458953514382464234321326889464182768467546703537516986049910576551282076245490090389328944075868508455133942304583236903222948165808559332123348274797826204144723168738177180919299881250404026184124858368e308",
        0x7fefffffffffffff,
    ),
    (
        "1.79769313486231580793728971405303415079934132710037826936173778980444968292764750946649017977587207096330286416692887910946555547851940402630657488671505820681908902000708383676273854845817711531764475730270069855571366959622842914819860834936475292719074168444365510704342711559699508093042880177904174497792e308",
        0x7fefffffffffffff,
    ),
    (
        "1.79769313486231580693728971405303415079934132710037826936173778980444968292764750946649017977587207096330286416692887910946555547851940402630657488671505820681908902000708383676273854845817711531764475730270069855571366959622842914819860834936475292719074168444365510704342711559699508093042880177904174497792e308",
        0x7fefffffffffffff,
    ),
    (
        "1.79769313486231580893728971405303415079934132710037826936173778980444968292764750946649017977587207096330286416692887910946555547851940402630657488671505820681908902000708383676273854845817711531764475730270069855571366959622842914819860834936475292719074168444365510704342711559699508093042880177904174497792e308",
        0x7ff0000000000000,
    ),
    ("1.7976931348623157e308", 0x7fefffffffffffff),
    ("1.7976931348623158e308", 0x7fefffffffffffff),
    ("1.7976931348623159e308", 0x7ff0000000000000),
    (
        "2.4703282292062327208828439643411068618252990130716238221279284125033775363510437593264991818081799618989828234772285886546332835517796989819938739800539093906315035659515570226392290858392449105184435931802849936536152500319370457678249219365623669863658480757001585769269903706311928279558551332927834338409351978015531246597263579574622766465272827220056374006485499977096599470454020828166226237857393450736339007967761930577506740176324673600968951340535537458516661134223766678604162159680461914467291840300530057530849048765391711386591646239524912623653881879636239373280423891018672348497668235089863388587925628302755995657524455507255189313690836254779186948667994968324049705821028513185451396213837722826145437693412532098591327667236328125e-324",
        0x0000000000000000,
    ),
    (
        "7.4109846876186981626485318930233205854758970392148714663837852375101326090531312779794975454245398856969484704316857659638998506553390969459816219401617281718945106978546710679176872575177347315553307795408549809608457500958111373034747658096871009590975442271004757307809711118935784838675653998783503015228055934046593739791790738723868299395818481660169122019456499931289798411362062484498678713572180352209017023903285791732520220528974020802906854021606612375549983402671300035812486479041385743401875520901590172592547146296175134159774938718574737870961645638908718119841271673056017045493004705269590165763776884908267986972573366521765567941072508764337560846003984904972149117463085539556354188641513168478436313080237596295773983001708984375e-324",
        0x0000000000000001,
    ),
    ("2.4703282292062327208e-324", 0x0000000000000000),
    ("2.4703282292062327209e-324", 0x0000000000000000),
    (
        "2.22507385850720113605740979670913197593481954635164564802342610972482222202107694551652952390813508791414915891303962110687008643869459464552765720740782062174337998814106326732925355228688137214901298112245145188984905722230728525513315575501591439747639798341180199932396254828901710708185069063066665599493827577257201576306269066333264756530000924588831643303777979186961204949739037782970490505108060994073026293712895895000358379996720725430436028407889577179615094551674824347103070260914462157228988025818254518032570701886087211312807951223342628836862232150377566662250398253433597456888442390026549819838548794829220689472168983109969836584681402285424333066033985088644580400103493397042756718644338377048603786162277173854562306587467901408672332763671875e-308",
        0x000fffffffffffff,
    ),
    (
        "2.2250738585072008890245868760858598876504231122409594654935248025624400092282356951787758888037591552642309780950434312085877387158357291821993020294379224223559819827501242041788969571311791082261043971979604000454897391938079198936081525613113376149842043271751033627391549782731594143828136275113838604094249464942286316695429105080201815926642134996606517803095075913058719846423906068637102005108723282784678843631944515866135041223479014792369585208321597621066375401613736583044193603714778355306682834535634005074073040135602968046375918583163124224521599262546494300836851861719422417646455137135420132217031370496583210154654068035397417906022589503023501937519773030945763173210852507299305089761582519159720757232455434770912461317493580281734466552734375e-308",
        0x000fffffffffffff,
    ),
    (
        "2.225073858507201383090232717332404064219215980462331830553327416887204434813918195854283159012511020564067339731035811005152434161553460108856012385377718821130777993532002330479610147442583636071921565046942503734208375250806650616658158948720491179968591639648500635908770118304874799780887753749949451580451605050915399856582470818645113537935804992115981085766051992433352114352390148795699609591288891602992641511063466313393663477586513029371762047325631781485664350872122828637642044846811407613911477062801689853244110024161447421618567166150540154285084716752901903161322778896729707373123334086988983175067838846926092773977972858659654941091369095406136467568702398678315290680984617210924625396728515625e-308",
        0x0010000000000000,
    ),
    ("9007199254740993.0", 0x4340000000000000),
    ("18446744073709553664", 0x43f0000000000000),
    ("18446744073709553665", 0x43f0000000000000),
];
#[test]
fn special_cases_have_exact_bits() {
    for (lit, want) in SPECIAL {
        let got = hipp::sql::lexer::parse_real(lit.as_bytes())
            .unwrap_or_else(|| panic!("parse_real({lit}) returned None"));
        assert_eq!(
            got.to_bits(),
            *want,
            "parse_real({lit}) = {:#018x}, want {want:#018x}",
            got.to_bits()
        );
    }
    // Signed zero: the literal path is unsigned, but the
    // evaluator's unary minus must produce -0.0.
    assert_eq!(hipp_select_bits("SELECT -0.0"), (-0.0f64).to_bits());
    assert_eq!(hipp_select_bits("SELECT 0.0"), 0.0f64.to_bits());
}

#[test]
fn cast_and_affinity_text_conversions_match_oracle() {
    // CAST('lit' AS REAL), CAST(lit AS REAL), and REAL-affinity
    // conversion of a text value on INSERT all convert decimal
    // text to a REAL; every path must land on the oracle's
    // bits. The corpus subset stresses long mantissas,
    // subnormals and the overflow boundary.
    let lits = [
        "0.1",
        "123456789.123456789",
        "0.9999999999999999",
        "1e308",
        "2.4703282292062327209e-324",
        "4.9e-324",
        "1.7976931348623157e308",
        "18446744073709551616",
        "0.30000000000000004",
        "-2.675",
        "+1.0000000000000002",
    ];
    for lit in lits {
        let path = common::temp_db("v12-cast");
        common::cli(
            &path,
            &format!(
                "CREATE TABLE r (v REAL); \
                 INSERT INTO r VALUES (CAST('{lit}' AS REAL));"
            ),
        );
        let want = block_on(async {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            let mut q = db.prepare("SELECT v FROM r").await.expect("prep");
            assert_eq!(q.step().await.expect("step"), Step::Row);
            q.column_f64(0).expect("real").to_bits()
        });
        assert_eq!(
            hipp_select_bits(&format!("SELECT CAST('{lit}' AS REAL)")),
            want,
            "CAST('{lit}' AS REAL)"
        );
        assert_eq!(
            hipp_select_bits(&format!("SELECT CAST({lit} AS REAL)")),
            want,
            "CAST({lit} AS REAL)"
        );
        // REAL affinity applied to a text value on INSERT.
        let got = block_on(async {
            let mut db = Connection::open(MemStorage::new(), Config::default())
                .await
                .expect("open");
            exec(&mut db, "CREATE TABLE r (v REAL)").await;
            exec(&mut db, &format!("INSERT INTO r VALUES ('{lit}')")).await;
            let mut q = db.prepare("SELECT v FROM r").await.expect("prep");
            assert_eq!(q.step().await.expect("step"), Step::Row);
            q.column_f64(0).expect("real").to_bits()
        });
        assert_eq!(got, want, "affinity conversion of '{lit}'");
    }
}

#[test]
fn stored_real_comparisons_match_oracle() {
    // Comparisons against literals are where a 1-ulp parse
    // error becomes a wrong row set. The table holds the
    // oracle's values; hipp must select the same rows with
    // literal predicates whose parse used to diverge.
    let path = common::temp_db("v12-cmp");
    common::cli(
        &path,
        "CREATE TABLE r (id INTEGER PRIMARY KEY, v REAL); \
         INSERT INTO r VALUES (1, 1.0), (2, 0.9999999999999999), \
         (3, 1e308), (4, 9.999999999999999e307), (5, 4.9e-324), \
         (6, 0.0), (7, 2.5e-324), (8, 9007199254740992.0), \
         (9, 9007199254740993.0);",
    );
    let predicates = [
        ("v = 1.0", "1"),
        ("v = 0.9999999999999999", "2"),
        ("v < 1.0 AND v > 0.9", "2"),
        ("v = 1e308", "3"),
        ("v = 9.999999999999999e307", "4"),
        ("v > 9.999999999999999e307", "3"),
        ("v = 4.9e-324", "5,7"),
        ("v = 2.5e-324", "5,7"),           // rounds to the min subnormal
        ("v = 2.47e-324", "6"),            // rounds to zero
        ("v = 9007199254740993.0", "8,9"), // ties to 2^53
        ("v > 9007199254740992.0", "3,4"),
        ("v >= 9007199254740992.0 AND v < 9007199254740994.0", "8,9"),
    ];
    block_on(async {
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        for (pred, want) in predicates {
            let mut q = db
                .prepare(&format!(
                    "SELECT group_concat(id) FROM (SELECT id FROM r WHERE {pred} ORDER BY id)"
                ))
                .await
                .expect("prep");
            assert_eq!(q.step().await.expect("step"), Step::Row);
            let got = q
                .column_bytes(0)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            assert_eq!(got, want, "WHERE {pred}");
        }
    });
    // And the same predicates against the oracle CLI, as a
    // cross-check that the expected sets are the pin's.
    for (pred, want) in predicates {
        let out = common::cli(
            &path,
            &format!("SELECT group_concat(id) FROM (SELECT id FROM r WHERE {pred} ORDER BY id);"),
        );
        assert_eq!(out.trim(), want, "oracle WHERE {pred}");
    }
}

#[test]
fn integer_overflow_literals_match_oracle() {
    // Plain integer literal text past i64 (and past u64)
    // becomes a REAL in both engines, correctly rounded,
    // including exact midpoints between adjacent floats.
    let lits = [
        "9223372036854775808",
        "9223372036854775809",
        "18446744073709551615",
        "18446744073709551616",
        "18446744073709551617", // 2^64 + 1 -> 2^64
        "18446744073709553664", // midpoint 2^64 .. 2^64+4096 -> tie to even
        "18446744073709553665", // just above the midpoint
        "18446744073709555712", // 2^64 + 4096 exactly
        "99999999999999999999999999999999999999999999999999999999",
        "340282366920938463463374607431768211456", // 2^128
        "340282366920938463463374607431768211455", // 2^128 - 1 -> 2^128
    ];
    let lits: Vec<String> = lits.iter().map(|s| s.to_string()).collect();
    let want = oracle_bits(&lits, "v12-intoverflow");
    for (lit, w) in lits.iter().zip(want.iter()) {
        assert_eq!(
            hipp_select_bits(&format!("SELECT {lit}")),
            *w,
            "integer literal {lit}"
        );
        assert_eq!(
            hipp_select_bits(&format!("SELECT -{lit}")),
            *w ^ (1 << 63),
            "integer literal -{lit}"
        );
    }
}

#[test]
fn exponent_extremes_and_zero_forms() {
    // Overflow to Inf, underflow to 0, signed zero exponents,
    // and zero mantissas with wild exponents.
    for (sql, want) in [
        ("SELECT 1e999", f64::INFINITY.to_bits()),
        ("SELECT -1e999", f64::NEG_INFINITY.to_bits()),
        ("SELECT 1e-999", 0.0f64.to_bits()),
        ("SELECT 0e999", 0.0f64.to_bits()),
        ("SELECT 0.0e-999", 0.0f64.to_bits()),
        ("SELECT 0e-999", 0.0f64.to_bits()),
        ("SELECT 5e-324", 1u64), // min subnormal
        ("SELECT 3e-324", 1u64),
        ("SELECT 2e-324", 0u64),
        ("SELECT 1.5e-323", 3u64), // 3 * 2^-1074 = 1.482e-323, nearest is 3
    ] {
        assert_eq!(hipp_select_bits(sql), want, "{sql}");
    }
    // The oracle agrees on the same extremes (via printf,
    // which prints Inf / the subnormal exactly enough here).
    let path = common::temp_db("v12-extremes");
    let out = common::cli(
        &path,
        "CREATE TABLE r (v REAL); INSERT INTO r VALUES (1e999), (1e-999), (5e-324); \
         SELECT printf('%!.25e', v) FROM r ORDER BY rowid;",
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Inf");
    assert_eq!(lines[1], "0.0e+00");
    assert_eq!(lines[2], "4.94065645841246544e-324");
}
