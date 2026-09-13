//! Micro-probes behind `bench/OPTIMISATION-CANDIDATES.md`.
//!
//! The matrix runner measures whole operations end to end, which is the right unit for
//! "is textdb competitive" but the wrong one for "where does the time go": a cell mixes
//! the engine, the SQL surface, the structure sidecar and the index writes into one
//! number. These probes isolate one cost at a time against a faithful copy of the
//! `sql-text-sqlite` schema in the same process, so a candidate can be sized before
//! anyone writes the optimisation.
//!
//! ```sh
//! cargo build --release --workspace
//! ./target/release/textdb-probe all          # every probe except `diff-big`
//! ./target/release/textdb-probe ops
//! ./target/release/textdb-probe diff-big     # allocates multiple GiB; can be OOM-killed
//! ```
//!
//! Absolute numbers are host-specific. The ratios within one run are the signal.

use std::time::{Duration, Instant};

use rusqlite::{params, Connection};

/// The `sql-text-sqlite` baseline, copied from `bench/harness/src/backends/sql_text_sqlite.rs`
/// so the comparison is against what the matrix actually compares against.
const BASE_SCHEMA: &str = r#"
CREATE TABLE doc (id INTEGER PRIMARY KEY, path TEXT NOT NULL, body TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 1, deleted INTEGER NOT NULL DEFAULT 0);
CREATE UNIQUE INDEX doc_path ON doc(path) WHERE deleted = 0;
CREATE TABLE doc_rev (doc_id INTEGER NOT NULL, version INTEGER NOT NULL, body TEXT NOT NULL,
  PRIMARY KEY (doc_id, version));
CREATE VIRTUAL TABLE doc_fts USING fts5(body, content='doc', content_rowid='id', tokenize='unicode61');
CREATE TRIGGER doc_ai AFTER INSERT ON doc BEGIN
  INSERT INTO doc_fts(rowid, body) VALUES (new.id, new.body);
  INSERT INTO doc_rev(doc_id, version, body) VALUES (new.id, new.version, new.body);
END;
CREATE TRIGGER doc_au AFTER UPDATE OF body ON doc BEGIN
  INSERT INTO doc_fts(doc_fts, rowid, body) VALUES ('delete', old.id, old.body);
  INSERT INTO doc_fts(rowid, body) VALUES (new.id, new.body);
  INSERT INTO doc_rev(doc_id, version, body) VALUES (new.id, new.version, new.body);
END;
"#;

const PRAGMAS: &str = "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=60000; \
                       PRAGMA cache_size=-262144; PRAGMA mmap_size=1073741824;";

/// Markdown with one `##` heading every 40 lines. `seed` makes every document distinct:
/// chunk sharing is the whole point of the store, so a probe that writes the same body
/// twice measures the dedupe path and not the write path.
fn corpus(nbytes: usize, seed: usize) -> String {
    let mut s = String::new();
    let mut i = 0usize;
    while s.len() < nbytes {
        if i % 40 == 0 {
            s.push_str(&format!("## Section {}-{}\n\n", seed, i / 40));
        }
        s.push_str(&format!(
            "line {:06}-{:05} lorem ipsum dolor sit amet consectetur adipiscing elit sed\n",
            i, seed
        ));
        i += 1;
    }
    s.truncate(nbytes);
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Change one line in the middle of the body.
fn edit_one_line(body: &str) -> String {
    let mut v = body.to_string();
    let needle = "line 000050";
    match v.find(needle) {
        Some(p) => v.replace_range(p..p + needle.len(), "LINE XXXXXX"),
        None => v.push_str("tail\n"),
    }
    v
}

/// `body` with every `every`-th line prefixed, i.e. `lines/every` scattered one-line edits.
fn scatter(body: &str, every: usize) -> String {
    let mut v = String::with_capacity(body.len() + body.len() / every);
    for (i, l) in body.lines().enumerate() {
        if i % every == 7 {
            v.push_str("CHANGED ");
        }
        v.push_str(l);
        v.push('\n');
    }
    v
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn timed(reps: usize, mut f: impl FnMut(usize)) -> f64 {
    let t = Instant::now();
    for i in 0..reps {
        f(i);
    }
    ms(t.elapsed()) / reps as f64
}

/// Peak resident set, in KiB. `0` where the platform does not report it.
fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}

struct Stores {
    td: Connection,
    base: Connection,
    _dir: tempfile::TempDir,
}

fn open_stores() -> anyhow::Result<Stores> {
    let dir = tempfile::tempdir()?;
    let td = Connection::open(dir.path().join("textdb.db"))?;
    td.execute_batch(PRAGMAS)?;
    textdb_sqlite::register(&td, "kb_")?;
    td.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');")?;
    let base = Connection::open(dir.path().join("sqltext.db"))?;
    base.execute_batch(PRAGMAS)?;
    base.execute_batch(BASE_SCHEMA)?;
    Ok(Stores { td, base, _dir: dir })
}

fn label(size: usize) -> String {
    if size >= 1 << 20 {
        format!("{}MiB", size >> 20)
    } else {
        format!("{}KiB", size >> 10)
    }
}

/// Per-operation cost against the plain-text baseline, at three document sizes, with
/// `.md` and `.txt` bodies separated so the markdown structure sidecar is visible on its
/// own. `record_commit` extracts sections and links for `.md`/`.markdown` only, and every
/// document the matrix generates is `.md`, so the sidecar is inside every published write
/// number without ever appearing as a line of its own.
fn probe_ops() -> anyhow::Result<()> {
    let s = open_stores()?;
    println!("## ops — textdb vs sql-text-sqlite, same process, unique content per document\n");
    println!(
        "{:>7} {:>5} {:>11} {:>11} {:>11} {:>9}  {}",
        "size", "n", "td .md", "td .txt", "baseline", "md/base", "operation"
    );
    let mut seed = 0usize;
    let mut next = |size: usize, n: usize| -> Vec<String> {
        (0..n)
            .map(|_| {
                seed += 1;
                corpus(size, seed)
            })
            .collect()
    };
    for (size, n) in [(8 << 10, 300usize), (100 << 10, 80), (1 << 20, 20)] {
        let (md, txt, bse) = (next(size, n), next(size, n), next(size, n));
        let l = label(size);

        let c_md = timed(n, |i| {
            s.td.execute(
                "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'probe')",
                params![format!("/{}/md/f{:05}.md", l, i), &md[i]],
            )
            .unwrap();
        });
        let c_tx = timed(n, |i| {
            s.td.execute(
                "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'probe')",
                params![format!("/{}/tx/f{:05}.txt", l, i), &txt[i]],
            )
            .unwrap();
        });
        let c_bs = timed(n, |i| {
            s.base
                .execute(
                    "INSERT INTO doc(path, body) VALUES (?1, ?2)",
                    params![format!("/{}/f{:05}", l, i), &bse[i]],
                )
                .unwrap();
        });
        println!(
            "{:>7} {:>5} {:>11.3} {:>11.3} {:>11.3} {:>8.2}x  create",
            l, n, c_md, c_tx, c_bs, c_md / c_bs
        );

        let md2: Vec<String> = md.iter().map(|b| edit_one_line(b)).collect();
        let tx2: Vec<String> = txt.iter().map(|b| edit_one_line(b)).collect();
        let bs2: Vec<String> = bse.iter().map(|b| edit_one_line(b)).collect();
        let r_md = timed(n, |i| {
            s.td.execute(
                "UPDATE kb SET content = ?1 WHERE path = ?2",
                params![&md2[i], format!("/{}/md/f{:05}.md", l, i)],
            )
            .unwrap();
        });
        let r_tx = timed(n, |i| {
            s.td.execute(
                "UPDATE kb SET content = ?1 WHERE path = ?2",
                params![&tx2[i], format!("/{}/tx/f{:05}.txt", l, i)],
            )
            .unwrap();
        });
        let r_bs = timed(n, |i| {
            s.base
                .execute(
                    "UPDATE doc SET body = ?1, version = version + 1 WHERE path = ?2",
                    params![&bs2[i], format!("/{}/f{:05}", l, i)],
                )
                .unwrap();
        });
        println!(
            "{:>7} {:>5} {:>11.3} {:>11.3} {:>11.3} {:>8.2}x  replace (one line)",
            l, n, r_md, r_tx, r_bs, r_md / r_bs
        );

        let reps = if size >= 1 << 20 { 200 } else { 3000 };
        let (p, q) = (format!("/{}/md/f00000.md", l), format!("/{}/f00000", l));
        // Warm the thread's document cache so this measures a steady-state read.
        let _: String = s.td.query_row("SELECT content FROM kb WHERE path = ?1", params![&p], |r| r.get(0))?;
        let d_r = timed(reps, |_| {
            let _: String = s.td.query_row("SELECT content FROM kb WHERE path = ?1", params![&p], |r| r.get(0)).unwrap();
        });
        let b_r = timed(reps, |_| {
            let _: String = s.base.query_row("SELECT body FROM doc WHERE path = ?1", params![&q], |r| r.get(0)).unwrap();
        });
        println!(
            "{:>7} {:>5} {:>11.4} {:>11} {:>11.4} {:>8.2}x  read (warm)",
            l, reps, d_r, "-", b_r, d_r / b_r
        );

        let vreps = reps.min(500);
        let d_v = timed(vreps, |_| {
            let _: String = s.td.query_row("SELECT textdb_content(?1, 1)", params![&p], |r| r.get(0)).unwrap();
        });
        let b_v = timed(vreps, |_| {
            let _: String = s
                .base
                .query_row(
                    "SELECT body FROM doc_rev WHERE doc_id = (SELECT id FROM doc WHERE path = ?1) AND version = 1",
                    params![&q],
                    |r| r.get(0),
                )
                .unwrap();
        });
        println!(
            "{:>7} {:>5} {:>11.4} {:>11} {:>11.4} {:>8.2}x  read_version(1)",
            l, vreps, d_v, "-", b_v, d_v / b_v
        );
    }
    println!();
    Ok(())
}

/// What the virtual table's `filter` pays for compiling its statement on every call.
/// `RESULTS.md` names statement compilation as the residual on `read`; this sizes it.
fn probe_statements() -> anyhow::Result<()> {
    let s = open_stores()?;
    let body = corpus(8 << 10, 1);
    s.td.execute(
        "INSERT INTO kb(path, content, author) VALUES ('/s/a.txt', ?1, 'probe')",
        params![&body],
    )?;
    s.base
        .execute("INSERT INTO doc(path, body) VALUES ('/s/a', ?1)", params![&body])?;
    // The exact statement `KbCursor::filter` builds for a path lookup (idx_num == 1).
    let sql = "SELECT id, path, name, kind, root, version, nbytes, nlines, updated_at, updated_by \
               FROM kb_node WHERE path = ?1 AND deleted_at IS NULL";
    let reps = 5000;
    let _: String = s.td.query_row("SELECT content FROM kb WHERE path = '/s/a.txt'", [], |r| r.get(0))?;
    let vtab = timed(reps, |_| {
        let _: String = s.td.query_row("SELECT content FROM kb WHERE path = '/s/a.txt'", [], |r| r.get(0)).unwrap();
    });
    let base = timed(reps, |_| {
        let _: String = s.base.query_row("SELECT body FROM doc WHERE path = '/s/a'", [], |r| r.get(0)).unwrap();
    });
    let cached = timed(reps, |_| {
        let _: i64 = s.td.prepare_cached(sql).unwrap().query_row(params!["/s/a.txt"], |r| r.get(0)).unwrap();
    });
    let fresh = timed(reps, |_| {
        let _: i64 = s.td.prepare(sql).unwrap().query_row(params!["/s/a.txt"], |r| r.get(0)).unwrap();
    });
    println!("## statements — 8 KiB document, warm caches\n");
    println!("  read through the virtual table        {:8.4} ms", vtab);
    println!("  read from the plain-text baseline     {:8.4} ms  ({:.2}x)", base, vtab / base);
    println!("  node-row lookup, prepare_cached       {:8.4} ms", cached);
    println!("  node-row lookup, prepare each time    {:8.4} ms", fresh);
    // The last two lines are what the virtual table's `filter` pays per call for its one
    // statement. While it called `prepare`, that difference *was* most of the gap above; it
    // now prepares through the table's own connection, so what remains of the gap is the
    // rest of the path. If `gap` ever creeps back up towards `compile` again, the statement
    // cache has stopped being reused.
    println!(
        "  -> compile cost {:.4} ms/call; gap to the baseline {:.4} ms\n",
        fresh - cached,
        vtab - base
    );
    Ok(())
}

/// `textdb_content` / `textdb_lines` are scalar SQL functions, which get a fresh
/// `Connection` per call and so cannot reuse a statement cache the way the virtual tables
/// now do. This splits their cost into the SQL entry point, the lookups and the work.
fn probe_scalar() -> anyhow::Result<()> {
    let s = open_stores()?;
    println!("## scalar — per-call cost of the scalar function entry point\n");
    for size in [8 << 10usize, 1 << 20] {
        let body = corpus(size, 11);
        let l = label(size);
        let path = format!("/{}/a.txt", l);
        s.td.execute(
            "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'probe')",
            params![&path, &body],
        )?;
        s.td.execute(
            "UPDATE kb SET content = ?1 WHERE path = ?2",
            params![edit_one_line(&body), &path],
        )?;
        s.base
            .execute("INSERT INTO doc(path, body) VALUES (?1, ?2)", params![&path, &body])?;
        let reps = if size >= 1 << 20 { 200 } else { 3000 };
        // Warm every cache these paths use.
        let _: String = s.td.query_row("SELECT textdb_content(?1, 1)", params![&path], |r| r.get(0))?;
        let _: String = s.td.query_row("SELECT content FROM kb WHERE path = ?1", params![&path], |r| r.get(0))?;

        let f_version = timed(reps, |_| {
            let _: String = s.td.query_row("SELECT textdb_content(?1, 1)", params![&path], |r| r.get(0)).unwrap();
        });
        let f_head = timed(reps, |_| {
            let _: String = s.td.query_row("SELECT textdb_content(?1)", params![&path], |r| r.get(0)).unwrap();
        });
        let vtab = timed(reps, |_| {
            let _: String = s.td.query_row("SELECT content FROM kb WHERE path = ?1", params![&path], |r| r.get(0)).unwrap();
        });
        let b_version = timed(reps, |_| {
            let _: String = s
                .base
                .query_row(
                    "SELECT body FROM doc_rev WHERE doc_id = (SELECT id FROM doc WHERE path = ?1) AND version = 1",
                    params![&path],
                    |r| r.get(0),
                )
                .unwrap();
        });
        // A scalar function that does no textdb work at all, for the floor.
        let floor = timed(reps, |_| {
            let _: i64 = s.td.query_row("SELECT length(?1)", params![&path], |r| r.get(0)).unwrap();
        });
        println!(
            "  {:>6}  textdb_content(path, 1) {:8.4} ms | textdb_content(path) {:8.4} | kb content column {:8.4} | baseline {:8.4} ({:.2}x) | bare scalar fn {:8.4}",
            l, f_version, f_head, vtab, b_version, f_version / b_version, floor
        );
    }
    println!();
    Ok(())
}

/// Prefix queries are written `substr(path, 1, length(?1) + 1) = ?1 || '/'`, which no
/// index can serve. The same predicate as a range over `path` uses `{p}node_path`.
fn probe_prefix() -> anyhow::Result<()> {
    let s = open_stores()?;
    for i in 0..2000 {
        s.td.execute(
            "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'probe')",
            params![format!("/wide/d{:02}/f{:05}.md", i % 50, i), "x\n"],
        )?;
    }
    let deep = "/a".repeat(1000);
    s.td.execute(
        "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'probe')",
        params![format!("{}/leaf.md", deep), "x\n"],
    )?;
    println!("## prefix — 2000 files plus one path of depth 1000\n");
    for (name, prefix) in [("shallow ('/wide/d00')", "/wide/d00".to_string()), ("deep (depth 1000)", deep)] {
        let scan = timed(20, |_| {
            let _: i64 = s
                .td
                .query_row(
                    "SELECT count(*) FROM kb_node WHERE deleted_at IS NULL \
                     AND substr(path, 1, length(?1) + 1) = ?1 || '/'",
                    params![&prefix],
                    |r| r.get(0),
                )
                .unwrap();
        });
        let range = timed(20, |_| {
            let _: i64 = s
                .td
                .query_row(
                    "SELECT count(*) FROM kb_node WHERE deleted_at IS NULL \
                     AND path >= ?1 || '/' AND path < ?1 || '0'",
                    params![&prefix],
                    |r| r.get(0),
                )
                .unwrap();
        });
        println!(
            "  {:22}  substr scan {:8.3} ms | indexed range {:8.3} ms  ({:.0}x)",
            name, scan, range, scan / range
        );
    }
    // Through the virtual table, which is how every caller actually lists a subtree. A
    // bound on `path` reaches `{p}node_path`; `substr(path, …)` cannot be a constraint at
    // all, so SQLite asks the cursor for every row and filters afterwards.
    for (name, prefix) in [("shallow ('/wide/d00')", "/wide/d00".to_string()), ("deep (depth 1000)", "/a".repeat(1000))] {
        let scan = timed(20, |_| {
            let _: i64 = s
                .td
                .query_row(
                    "SELECT count(*) FROM kb WHERE substr(path, 1, length(?1) + 1) = ?1 || '/'",
                    params![&prefix],
                    |r| r.get(0),
                )
                .unwrap();
        });
        let range = timed(20, |_| {
            let _: i64 = s
                .td
                .query_row(
                    "SELECT count(*) FROM kb WHERE path >= ?1 || '/' AND path < ?1 || '0'",
                    params![&prefix],
                    |r| r.get(0),
                )
                .unwrap();
        });
        println!(
            "  via kb {:22}  substr scan {:8.3} ms | pushed-down range {:8.3} ms  ({:.0}x)",
            name, scan, range, scan / range
        );
    }
    // `ensure_folder` issues a lookup and an insert per missing component.
    let deep2 = "/b".repeat(1000);
    let t = Instant::now();
    s.td.execute(
        "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'probe')",
        params![format!("{}/leaf.md", deep2), "x\n"],
    )?;
    println!("  create at depth 1000 (1000 folder rows) {:8.2} ms\n", ms(t.elapsed()));
    Ok(())
}

/// Per-chunk SQL on the write path: a 1 MiB document is ~700 chunks, and each one costs a
/// `chunk` insert, an `fts` insert and a `chunk_ref` insert that resolves the hash back to
/// a rowid. The baseline writes one row and indexes one document.
fn probe_writepath() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("w.db"))?;
    conn.execute_batch(PRAGMAS)?;
    conn.execute_batch(
        "CREATE TABLE chunk(id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE, bytes BLOB NOT NULL, nlines INT NOT NULL);
         CREATE VIRTUAL TABLE fts USING fts5(text, content='', tokenize='unicode61');
         CREATE TABLE chunk_ref(chunk_id INT NOT NULL, file_id INT NOT NULL, version INT NOT NULL,
           PRIMARY KEY(chunk_id, file_id)) WITHOUT ROWID;",
    )?;
    let body = corpus(1 << 20, 7).into_bytes();
    let cuts = textdb_core::chunker::chunk_all(&textdb_core::ChunkParams::DEFAULT, &body);
    println!("## writepath — one 1 MiB document, {} chunks\n", cuts.len());

    conn.execute_batch("BEGIN")?;
    let t = Instant::now();
    for (s, l) in &cuts {
        let b = &body[*s..*s + *l];
        let h = textdb_core::hash::hash_chunk(b);
        conn.prepare_cached("INSERT OR IGNORE INTO chunk(hash, bytes, nlines) VALUES (?1, ?2, ?3)")?
            .execute(params![&h[..], b, textdb_core::chunker::count_newlines(b) as i64])?;
    }
    let t_chunk = ms(t.elapsed());
    conn.execute_batch("COMMIT")?;

    conn.execute_batch("BEGIN")?;
    let t = Instant::now();
    for (i, (s, l)) in cuts.iter().enumerate() {
        let b = &body[*s..*s + *l];
        conn.prepare_cached("INSERT INTO fts(rowid, text) VALUES (?1, ?2)")?
            .execute(params![i as i64 + 1, String::from_utf8_lossy(b).as_ref()])?;
    }
    let t_fts = ms(t.elapsed());
    conn.execute_batch("COMMIT")?;

    conn.execute_batch("BEGIN")?;
    let t = Instant::now();
    for (s, l) in &cuts {
        let h = textdb_core::hash::hash_chunk(&body[*s..*s + *l]);
        conn.prepare_cached(
            "INSERT OR IGNORE INTO chunk_ref(chunk_id, file_id, version) SELECT id, ?2, ?3 FROM chunk WHERE hash = ?1",
        )?
        .execute(params![&h[..], 1i64, 1i64])?;
    }
    let t_ref = ms(t.elapsed());
    conn.execute_batch("COMMIT")?;

    let t = Instant::now();
    let mut mem = textdb_core::storage::MemStorage::new();
    textdb_core::build_with_chunks(&mut mem, &textdb_core::ChunkParams::DEFAULT, &body)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let t_cpu = ms(t.elapsed());
    println!("  chunk + hash + tree build, in memory  {:8.2} ms", t_cpu);
    println!("  chunk inserts                         {:8.2} ms", t_chunk);
    println!("  fts inserts                           {:8.2} ms", t_fts);
    println!("  chunk_ref (hash -> rowid, insert)     {:8.2} ms\n", t_ref);
    Ok(())
}

/// How much of a tree walk is the walk, and how much is the bytes?
///
/// `Storage::get_node` returns `Node` by value, so every node access clones a `Vec<Child>`
/// of up to `MAX_FANOUT` entries, and `tree::Cursor` holds nodes by value too, so cloning a
/// cursor deep-copies its whole path. An `Arc<Node>` would make both pointer bumps — the
/// same move `chunk_shared` already made for chunk bytes. Whether that is worth doing is a
/// question about the ratio below, not a matter of taste: if materialising a document runs
/// near memcpy speed then the walk is not where the time is.
fn probe_tree() -> anyhow::Result<()> {
    use textdb_core::{storage::MemStorage, ChunkParams};
    println!("## tree — walk overhead against the cost of the bytes themselves\n");
    // `MemStorage` takes the default `chunk_shared`, which allocates a fresh `Arc` and copies
    // the chunk into it on every leaf. `SqliteStorage` overrides that and hands out the cached
    // `Arc`, so the materialize column here is an upper bound on the real path, not a
    // measurement of it. It is still the right vehicle for the edit scaling below, which is
    // about the algorithm rather than the binding.
    println!("  (materialize over MemStorage, whose default chunk_shared copies per leaf: an upper bound)");
    for size in [1usize << 20, 8 << 20, 32 << 20] {
        let body = corpus(size, 5).into_bytes();
        let mut mem = MemStorage::new();
        let root = textdb_core::build(&mut mem, &ChunkParams::DEFAULT, &body).map_err(|e| anyhow::anyhow!("{e}"))?;
        let leaves = textdb_core::tree::leaves(&mem, &root).map_err(|e| anyhow::anyhow!("{e}"))?.len();
        let depth = textdb_core::tree::depth(&mem, &root).map_err(|e| anyhow::anyhow!("{e}"))?;
        let reps = (64 << 20) / size;
        let walk = timed(reps, |_| {
            let v = textdb_core::materialize(&mem, &root).unwrap();
            assert_eq!(std::hint::black_box(&v).len(), body.len());
        });
        // The floor: one allocation and one copy of the same bytes, no tree involved.
        // `black_box` both ways, or the optimiser deletes a clone nobody reads.
        let copy = timed(reps, |_| {
            let v = std::hint::black_box(&body).clone();
            assert_eq!(std::hint::black_box(&v).len(), body.len());
        });
        println!(
            "  {:>6}  {:6} leaves, depth {}, {:4} internal nodes | materialize {:8.3} ms ({:.0} MB/s) | plain copy {:8.3} ms ({:.0} MB/s) | {:.2}x",
            label(size),
            leaves,
            depth,
            mem.nodes.len(),
            walk,
            size as f64 / 1e6 / (walk / 1e3),
            copy,
            size as f64 / 1e6 / (copy / 1e3),
            walk / copy
        );
    }
    // The edit path is where cursors get cloned: `apply_one` clones a whole `Cursor` — and
    // so every `Node` on its path — for each suffix leaf it pulls into the re-chunk window,
    // and `rebuild_level` clones the path again per level. If that mattered, the cost of one
    // small edit would grow with the document. Claim O1 says it should not.
    println!("\n  one 10-byte edit in the middle, by document size (claim O1: cost of the edit, not the file)");
    for size in [1usize << 20, 8 << 20, 32 << 20] {
        let body = corpus(size, 6).into_bytes();
        let mut mem = MemStorage::new();
        let root = textdb_core::build(&mut mem, &ChunkParams::DEFAULT, &body).map_err(|e| anyhow::anyhow!("{e}"))?;
        let at = (size / 2) as u64;
        let depth = textdb_core::tree::depth(&mem, &root).map_err(|e| anyhow::anyhow!("{e}"))?;
        let reps = 200;
        let edit = timed(reps, |_| {
            let er = textdb_core::edit::apply_edits(
                &mut mem,
                &ChunkParams::DEFAULT,
                &root,
                &[textdb_core::Edit::new(at, at + 10, b"CHANGED".to_vec())],
            )
            .unwrap();
            std::hint::black_box(er.root);
        });
        println!("    {:>6}  depth {}  apply_edits {:8.4} ms", label(size), depth, edit);
    }
    println!();
    Ok(())
}

/// `myers::byte_edits` runs on every whole-document write (`UPDATE kb SET content = …`).
/// Its V array and its per-diagonal trace are sized by the inputs rather than by the
/// distance bound, so cost grows with the *square* of the number of changed lines.
fn probe_diff(big: bool) -> anyhow::Result<()> {
    let sizes: &[usize] = if big { &[1 << 20, 8 << 20] } else { &[1 << 20] };
    println!("## diff — byte_edits over scattered one-line changes\n");
    if big {
        println!("  (`diff-big` allocates several GiB at the larger counts and may be OOM-killed)\n");
    }
    for &size in sizes {
        let body = corpus(size, 3);
        let lines = body.lines().count();
        println!("  {} document, {} lines", label(size), lines);
        let everys: &[usize] = if size <= (1 << 20) { &[2000, 500, 100, 25] } else { &[2000, 500] };
        for &every in everys {
            let other = scatter(&body, every);
            let before = peak_rss_kb();
            let t = Instant::now();
            let edits = textdb_core::myers::byte_edits(body.as_bytes(), other.as_bytes());
            println!(
                "    ~{:5} changed lines -> {:5} edits  {:9.1} ms  peak RSS +{} KiB",
                lines / every,
                edits.len(),
                ms(t.elapsed()),
                peak_rss_kb().saturating_sub(before)
            );
        }
    }
    println!();
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    match which.as_str() {
        "ops" => probe_ops()?,
        "statements" => probe_statements()?,
        "scalar" => probe_scalar()?,
        "prefix" => probe_prefix()?,
        "writepath" => probe_writepath()?,
        "tree" => probe_tree()?,
        "diff" => probe_diff(false)?,
        "diff-big" => probe_diff(true)?,
        "all" => {
            probe_ops()?;
            probe_statements()?;
            probe_scalar()?;
            probe_prefix()?;
            probe_writepath()?;
            probe_tree()?;
            probe_diff(false)?;
        }
        other => {
            eprintln!("unknown probe '{}'", other);
            eprintln!("usage: textdb-probe [all|ops|statements|scalar|prefix|writepath|tree|diff|diff-big]");
            std::process::exit(2);
        }
    }
    Ok(())
}
