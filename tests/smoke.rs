//! First vertical slice: create/insert/select round trip on
//! MemStorage, reopen from snapshot bytes.

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, OwnedValue, Step};

type E = hipp::storage::MemError;

async fn run() -> Result<(), hipp::Error<E>> {
    let mem = MemStorage::new();
    let mut db = Connection::open(mem.clone(), Config::default()).await?;
    {
        let mut st = db
            .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL, b BLOB)")
            .await?;
        assert_eq!(st.step().await?, Step::Done);
    }
    let rows = [
        "INSERT INTO t VALUES (1, 'one', 1.5, x'00ff')",
        "INSERT INTO t VALUES (9007199254740993, 'big', NULL, x'')",
        "INSERT INTO t (name, id) VALUES ('auto', NULL)",
        "INSERT INTO t VALUES (4, '', -2.25, NULL)",
    ];
    for sql in rows {
        let mut st = db.prepare(sql).await?;
        assert_eq!(st.step().await?, Step::Done);
    }
    let snap = mem.snapshot();
    assert!(snap.len() >= 8192, "two pages minimum, got {}", snap.len());

    // Reopen on the snapshot.
    let mem2 = MemStorage::new();
    mem2.set_snapshot(snap);
    let mut db = Connection::open(mem2, Config::default()).await?;
    let mut q = db.prepare("SELECT id, name, v, b FROM t").await?;
    let mut seen = Vec::new();
    while q.step().await? == Step::Row {
        seen.push((
            q.column_i64(0),
            q.column_bytes(1).map(|b| b.to_vec()),
            q.column_f64(2),
            q.column_value(3).map(|v| match v {
                hipp::Value::Blob(b) => Some(b.to_vec()),
                hipp::Value::Null => None,
                _ => panic!("unexpected {v:?}"),
            }),
        ));
    }
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].0, Some(1));
    assert_eq!(seen[0].1.as_deref(), Some(b"one".as_slice()));
    assert_eq!(seen[0].2, Some(1.5));
    assert_eq!(seen[0].3, Some(Some(vec![0x00, 0xff])));
    // Scan order is rowid order: 1, 4, 9007199254740993, ...994.
    assert_eq!(seen[1].0, Some(4));
    assert_eq!(seen[1].1.as_deref(), Some(b"".as_slice())); // empty text
    assert_eq!(seen[1].2, Some(-2.25));
    assert_eq!(seen[1].3, Some(None)); // NULL blob column
    assert_eq!(seen[2].0, Some(9007199254740993));
    assert_eq!(seen[2].3, Some(Some(vec![]))); // empty blob, not NULL
    // 'auto' was inserted when max rowid was 9007199254740993, so
    // it auto-assigned ...994 (explicit rowid 4 came after).
    let auto = seen.iter().find(|r| r.1.as_deref() == Some(b"auto".as_slice())).unwrap();
    assert_eq!(auto.0, Some(9007199254740994));
    // Rowid order: 1, 4, big, auto
    let ids: Vec<_> = seen.iter().map(|r| r.0).collect();
    assert_eq!(
        ids,
        vec![Some(1), Some(4), Some(9007199254740993), Some(9007199254740994)]
    );
    Ok(())
}

#[test]
fn mem_round_trip() {
    block_on(run()).expect("round trip");
}

#[test]
fn owned_value_sanity() {
    let v = OwnedValue::from_value(hipp::Value::Integer(7));
    assert_eq!(v.as_value(), hipp::Value::Integer(7));
}
