//! Page splits around large cells. A table leaf cell can hold up to nearly a
//! whole page of payload before it spills to overflow pages, so a row that
//! grows in the middle of a full leaf can leave no way to halve the page by
//! bytes: the half that takes the big cell is still too big. Every result is
//! checked against the `sqlite3` CLI's integrity check and row contents.

mod common;

use common::*;
use makepad_sqlite::{Connection, Value};
use std::path::Path;
use std::time::Duration;

fn open(path: &Path) -> Connection {
    Connection::open(path, Duration::from_secs(5)).expect("open")
}

fn cli(path: &Path, sql: &str) -> String {
    sqlite3(path, &format!(".mode list\n.headers off\n{sql}\n"))
        .trim()
        .to_string()
}

fn integrity(path: &Path) {
    assert_eq!(cli(path, "PRAGMA integrity_check;"), "ok");
}

/// Rows `(id, length)` of `body`, as the CLI reads them.
fn lengths(path: &Path) -> String {
    cli(path, "SELECT id, length(body) FROM t ORDER BY id;")
}

fn table_with(path: &Path, rows: &[(i64, usize)]) -> Connection {
    let mut db = open(path);
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    for (id, n) in rows {
        db.execute(
            "INSERT INTO t(id, body) VALUES (?, ?)",
            &[Value::Integer(*id), Value::text("x".repeat(*n).as_str())],
        )
        .unwrap();
    }
    db
}

#[test]
fn a_grown_middle_row_that_halving_cannot_place_splits_unevenly() {
    if !have_sqlite3() {
        return;
    }
    let scratch = Scratch::new("split-uneven");
    let path = scratch.path("uneven.db");
    {
        // Halving by bytes puts 1200 + 3000 on the left, which does not fit;
        // 1200 | 3000 + 300 does.
        let mut db = table_with(&path, &[(1, 1200), (2, 400), (3, 300)]);
        db.execute(
            "UPDATE t SET body = ? WHERE id = 2",
            &[Value::text("y".repeat(3000).as_str())],
        )
        .unwrap();
    }
    integrity(&path);
    assert_eq!(lengths(&path), "1|1200\n2|3000\n3|300");
}

#[test]
fn a_grown_middle_row_between_two_large_rows_splits_three_ways() {
    if !have_sqlite3() {
        return;
    }
    let scratch = Scratch::new("split-three");
    let path = scratch.path("three.db");
    {
        // No two-way split fits 1800 | 3900 | 1800: the big cell needs a
        // page of its own.
        let mut db = table_with(&path, &[(1, 1800), (2, 400), (3, 1800)]);
        db.execute(
            "UPDATE t SET body = ? WHERE id = 2",
            &[Value::text("y".repeat(3900).as_str())],
        )
        .unwrap();
    }
    integrity(&path);
    assert_eq!(lengths(&path), "1|1800\n2|3900\n3|1800");
}

#[test]
fn rows_of_every_size_survive_random_growth_and_shrinking() {
    if !have_sqlite3() {
        return;
    }
    let scratch = Scratch::new("split-random");
    let path = scratch.path("random.db");
    // A fixed linear congruential sequence, so a failure replays.
    let mut seed: u64 = 0x5eed_cafe;
    let mut next = move |below: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % below
    };
    let mut want: Vec<usize> = (0..300).map(|_| next(4000) as usize).collect();
    {
        let rows: Vec<(i64, usize)> = want.iter().enumerate().map(|(i, n)| (i as i64 + 1, *n)).collect();
        let mut db = table_with(&path, &rows);
        db.execute("BEGIN IMMEDIATE", &[]).unwrap();
        for _ in 0..2000 {
            let at = next(want.len() as u64) as usize;
            // Mostly page-sized rows, where the splits go wrong, some small.
            let n = if next(4) == 0 { next(200) } else { 2000 + next(2000) } as usize;
            db.execute(
                "UPDATE t SET body = ? WHERE id = ?",
                &[Value::text("z".repeat(n).as_str()), Value::Integer(at as i64 + 1)],
            )
            .unwrap();
            want[at] = n;
        }
        db.execute("COMMIT", &[]).unwrap();
    }
    integrity(&path);
    let expected: Vec<String> = want.iter().enumerate().map(|(i, n)| format!("{}|{}", i + 1, n)).collect();
    assert_eq!(lengths(&path), expected.join("\n"));
}
