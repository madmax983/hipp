//! TEMPORARY CI probe (removed in the follow-up commit): prints the
//! pinned oracle's substr() answers on extreme integer arguments so the
//! rule can be fitted. Fails on purpose so the output is shown.
mod common;

#[test]
fn zz_pin_probe_substr() {
    let db = common::temp_db("probe-substr");
    let starts = [
        "-9223372036854775808", "-9223372036854775807", "-9223372036854775806",
        "-4611686018427387904", "-2147483649", "-2147483648", "-1000000001", "-1000000000",
        "-999999999", "-27", "-26", "-25", "-1", "0", "1", "25", "26", "27",
        "2147483648", "9223372036854775807",
    ];
    let lens = ["", ", 3", ", -3", ", 9223372036854775807", ", -9223372036854775808", ", -9223372036854775807", ", 2147483648", ", -2147483649", ", 0"];
    let mut sql = String::new();
    for s in starts {
        for l in lens {
            sql.push_str(&format!(
                "SELECT '{s}{l}', quote(substr('abcdefghijklmnopqrstuvwxyz', {s}{l})), quote(substr(x'000102030405060708090a0b0c0d0e0f10111213141516171819', {s}{l}));\n"
            ));
        }
    }
    let out = common::cli(&db, &sql);
    println!("ORACLE {}\n{out}", common::oracle_version());
    panic!("probe output above (intentional)");
}
