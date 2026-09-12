//! Concurrent read-modify-write on a single line, through the SQL surface.
//!
//! Reproduces the CW-03 finding in `bench/RESULTS.md`: with many writers incrementing one
//! counter line, `textdb-sqlite` records far more commits than the counter advances, while
//! the OCC baseline in the benchmark is exact. The benchmark measures it but cannot say
//! why, because it only sees per-write outcomes. This test records the head counter after
//! every committed write, so a value that goes *backwards* is caught with the transition
//! that produced it — the difference between "an update was dropped" and "a stale value
//! overwrote a newer one", which point at different bugs.
//!
//! Writers use the same path the benchmark's `textdb-sqlite` backend does: read a version,
//! splice the increment into the content *of that version*, and submit the whole body with
//! `base_version` set, leaving the engine to diff, rebase and commit.

use rusqlite::{params, Connection};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

const PATH: &str = "/counter.md";
const WRITERS: usize = 50;
const OPS: usize = 20;

fn open(file: &std::path::Path) -> Connection {
    let conn = Connection::open(file).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 60000;")
        .unwrap();
    textdb_sqlite::register(&conn, "kb_").unwrap();
    conn
}

fn counter_of(body: &str) -> Option<u64> {
    body.lines()
        .find_map(|l| l.strip_prefix("count: "))
        .and_then(|t| t.split_whitespace().next())
        .and_then(|t| t.parse().ok())
}

#[test]
fn concurrent_counter_never_regresses() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kb.db");
    {
        let conn = open(&file);
        conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');")
            .unwrap();
        conn.execute(
            "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'seed')",
            params![PATH, "# counter\n\ncount: 0\ntail\n"],
        )
        .unwrap();
    }

    let committed = AtomicU64::new(0);
    // Every observed decrease of the head counter, as (before, after).
    let regressions: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
    let highest = AtomicU64::new(0);

    std::thread::scope(|s| {
        for w in 0..WRITERS {
            let committed = &committed;
            let regressions = &regressions;
            let highest = &highest;
            let file = &file;
            s.spawn(move || {
                let conn = open(file);
                for _ in 0..OPS {
                    // What this writer last saw; stale as soon as anyone else commits.
                    let (seen, ver): (String, i64) = match conn.query_row(
                        "SELECT content, version FROM kb WHERE path = ?1",
                        params![PATH],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    ) {
                        Ok(x) => x,
                        Err(_) => continue,
                    };
                    let Some(cur) = counter_of(&seen) else { continue };
                    let next = seen.replace(&format!("count: {}", cur), &format!("count: {}", cur + 1));
                    let tag = format!("w{}-{}", w, cur);
                    let wrote = conn.execute(
                        "UPDATE kb SET content = ?1, base_version = ?2, author = ?4 WHERE path = ?3",
                        params![next, ver, PATH, tag],
                    );
                    if wrote.is_err() {
                        continue; // conflict (TX001) or contention (TX002): the writer retries
                    }
                    // Did this write create a version of its own?
                    let mine: Option<i64> = conn
                        .query_row(
                            "SELECT max(version) FROM textdb_history(?1) WHERE author = ?2",
                            params![PATH, tag],
                            |r| r.get(0),
                        )
                        .unwrap_or(None);
                    if mine.is_none() {
                        continue; // absorbed: an identical concurrent edit already landed
                    }
                    committed.fetch_add(1, Ordering::Relaxed);
                    // The head counter right after our commit. Racy by nature, which is
                    // fine: we only ever claim a regression against a value already seen.
                    if let Ok(body) = conn.query_row("SELECT content FROM kb WHERE path = ?1", params![PATH], |r| {
                        r.get::<_, String>(0)
                    }) {
                        if let Some(now) = counter_of(&body) {
                            let prev = highest.fetch_max(now, Ordering::SeqCst);
                            if now < prev {
                                regressions.lock().unwrap().push((prev, now));
                            }
                        }
                    }
                }
            });
        }
    });

    let conn = open(&file);
    let body: String = conn
        .query_row("SELECT content FROM kb WHERE path = ?1", params![PATH], |r| r.get(0))
        .unwrap();
    let final_count = counter_of(&body).expect("counter line survived");
    let committed = committed.load(Ordering::Relaxed);
    let regressions = regressions.into_inner().unwrap();
    let peak = highest.load(Ordering::SeqCst);

    eprintln!(
        "writers={} ops={} committed={} final={} peak={} regressions={}",
        WRITERS,
        OPS,
        committed,
        final_count,
        peak,
        regressions.len()
    );
    if let Some((a, b)) = regressions.first() {
        eprintln!("first regression: {} -> {} ({} total)", a, b, regressions.len());
    }

    // The counter must never move backwards: a committed write that lowers it has
    // overwritten a newer value with a stale one.
    assert!(
        regressions.is_empty(),
        "counter regressed {} times, first {:?}; committed={} final={} peak={}",
        regressions.len(),
        regressions.first(),
        committed,
        final_count,
        peak
    );
    // And every commit that produced a version must have advanced it by one.
    assert_eq!(
        committed, final_count,
        "{} commits produced a final counter of {} (peak {})",
        committed, final_count, peak
    );
}
