//! The whole surface through SQL (spec claim 4) plus round-trip checks.

use rusqlite::{params, Connection, OptionalExtension};
use textdb_sqlite::{open_in_memory, TextDb};

fn setup() -> Connection {
    let conn = open_in_memory().unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
    conn
}

#[test]
fn create_read_update_history_diff() {
    let conn = setup();
    conn.execute(
        "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'alice')",
        params!["/notes/a.md", "# Title\n\nalpha\nbeta\ngamma\n"],
    )
    .unwrap();
    let (content, version, kind, parent): (String, i64, String, String) = conn
        .query_row(
            "SELECT content, version, kind, dir FROM kb WHERE path = '/notes/a.md'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(content, "# Title\n\nalpha\nbeta\ngamma\n");
    assert_eq!((version, kind.as_str(), parent.as_str()), (1, "file", "/notes"));
    // Parent folders were created.
    let folders: i64 = conn
        .query_row("SELECT count(*) FROM kb WHERE kind = 'folder'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(folders, 1);

    conn.execute(
        "UPDATE kb SET content = replace(content, 'beta', 'BETA'), author = 'bob' WHERE path = '/notes/a.md'",
        [],
    )
    .unwrap();
    let v: i64 = conn
        .query_row("SELECT version FROM kb WHERE path = '/notes/a.md'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
    let c1: String = conn
        .query_row("SELECT textdb_content('/notes/a.md', 1)", [], |r| r.get(0))
        .unwrap();
    assert!(c1.contains("beta"));
    let hist: Vec<(i64, Option<String>)> = conn
        .prepare("SELECT version, author FROM textdb_history('/notes/a.md')")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(hist, vec![(1, Some("alice".into())), (2, Some("bob".into()))]);
    let diff: String = conn
        .query_row("SELECT textdb_diff('/notes/a.md', 1, 2)", [], |r| r.get(0))
        .unwrap();
    assert!(diff.contains("-beta\n+BETA\n"), "{}", diff);
    // No-op update creates no version.
    conn.execute("UPDATE kb SET content = content WHERE path = '/notes/a.md'", []).unwrap();
    let v: i64 = conn
        .query_row("SELECT version FROM kb WHERE path = '/notes/a.md'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
    // Path rename does not materialize content and does not bump version.
    conn.execute("UPDATE kb SET path = '/notes/b.md' WHERE path = '/notes/a.md'", []).unwrap();
    let (v, id): (i64, i64) = conn
        .query_row("SELECT version, id FROM kb WHERE path = '/notes/b.md'", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(v, 2);
    assert_eq!(id, 3); // ids: 1 = "/", 2 = "/notes", 3 = the file
    // History follows the rename.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM textdb_history('/notes/b.md')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    // lines / section.
    let l: String = conn
        .query_row("SELECT textdb_lines('/notes/b.md', 3, 4)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(l, "alpha\nBETA\n");
    let s: String = conn
        .query_row("SELECT textdb_section('/notes/b.md', 'Title')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(s, "# Title\n\nalpha\nBETA\ngamma\n");
    // edit() strict replace and append().
    let v: i64 = conn
        .query_row("SELECT textdb_edit('/notes/b.md', 'gamma', 'delta')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 3);
    let err = conn
        .query_row("SELECT textdb_edit('/notes/b.md', 'nope', 'x')", [], |r| r.get::<_, i64>(0))
        .unwrap_err();
    assert!(err.to_string().contains("TX004"), "{}", err);
    let v: i64 = conn
        .query_row("SELECT textdb_append('/notes/b.md', 'tail\n')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 4);
    let c: String = conn.query_row("SELECT textdb_content('/notes/b.md')", [], |r| r.get(0)).unwrap();
    assert_eq!(c, "# Title\n\nalpha\nBETA\ndelta\ntail\n");
}

#[test]
fn rebase_and_conflict_through_base_version() {
    let conn = setup();
    conn.execute(
        "INSERT INTO kb(path, content) VALUES ('/f.md', ?1)",
        params!["line one\nline two\nline three\nline four\n"],
    )
    .unwrap();
    // Two agents read version 1; agent A changes line one, agent B changes line four.
    conn.execute(
        "UPDATE kb SET content = ?1, base_version = 1 WHERE path = '/f.md'",
        params!["LINE ONE\nline two\nline three\nline four\n"],
    )
    .unwrap();
    conn.execute(
        "UPDATE kb SET content = ?1, base_version = 1 WHERE path = '/f.md'",
        params!["line one\nline two\nline three\nLINE FOUR\n"],
    )
    .unwrap();
    let c: String = conn.query_row("SELECT content FROM kb WHERE path = '/f.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(c, "LINE ONE\nline two\nline three\nLINE FOUR\n");
    let v: i64 = conn.query_row("SELECT version FROM kb WHERE path = '/f.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 3);
    // Agent C also based on v1 changes line one differently → conflict TX001 with payload.
    let err = conn
        .execute(
            "UPDATE kb SET content = ?1, base_version = 1 WHERE path = '/f.md'",
            params!["Line 1\nline two\nline three\nline four\n"],
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("TX001"), "{}", msg);
    assert!(msg.contains("\"theirs\":\"LINE ONE\\n\""), "{}", msg);
    // Content unchanged after the failed commit.
    let c: String = conn.query_row("SELECT content FROM kb WHERE path = '/f.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(c, "LINE ONE\nline two\nline three\nLINE FOUR\n");
}

#[test]
fn folders_rename_delete_ls_export_search() {
    let conn = setup();
    for i in 0..20 {
        conn.execute(
            "INSERT INTO kb(path, content) VALUES (?1, ?2)",
            params![
                format!("/a/sub{}/doc{}.md", i % 3, i),
                format!("# Doc {}\n\nThe quick brown fox {} jumps.\n\nwikilink [[Doc {}]]\n", i, i, (i + 1) % 20)
            ],
        )
        .unwrap();
    }
    let ls: Vec<(String, String)> = conn
        .prepare("SELECT name, kind FROM textdb_ls('/a')")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        ls,
        vec![
            ("sub0".to_string(), "folder".to_string()),
            ("sub1".to_string(), "folder".to_string()),
            ("sub2".to_string(), "folder".to_string())
        ]
    );
    // Search: 2-term AND, prefix-restricted.
    let hits: Vec<(String, i64, String)> = conn
        .prepare("SELECT path, line, text FROM textdb_search('quick fox', '/a/sub1')")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h.0.starts_with("/a/sub1/") && h.1 == 3 && h.2.contains("quick brown fox")), "{:?}", hits);
    let one: Vec<String> = conn
        .prepare("SELECT path FROM textdb_search('\"fox 7\"', '/')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(one, vec!["/a/sub1/doc7.md".to_string()]);
    // Folder rename rewrites the subtree and keeps ids/versions.
    let before: Vec<(i64, i64)> = conn
        .prepare("SELECT id, version FROM kb WHERE path LIKE '/a/sub1/%' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    conn.execute("UPDATE kb SET path = '/b/moved' WHERE path = '/a/sub1'", []).unwrap();
    let after: Vec<(i64, i64)> = conn
        .prepare("SELECT id, version FROM kb WHERE path LIKE '/b/moved/%' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(before.len(), 7);
    let gone: i64 = conn
        .query_row("SELECT count(*) FROM kb WHERE path LIKE '/a/sub1%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gone, 0);
    // Search still finds moved content at the new path.
    let moved: Vec<String> = conn
        .prepare("SELECT path FROM textdb_search('\"fox 7\"', '/')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(moved, vec!["/b/moved/doc7.md".to_string()]);
    // Delete a folder: tombstoned, history and old versions readable.
    conn.execute("DELETE FROM kb WHERE path = '/b/moved'", []).unwrap();
    let n: i64 = conn.query_row("SELECT count(*) FROM kb WHERE path LIKE '/b/moved%'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    let old: String = conn
        .query_row("SELECT textdb_content('/b/moved/doc7.md', 1)", [], |r| r.get(0))
        .unwrap();
    assert!(old.contains("fox 7"));
    let none: Vec<String> = conn
        .prepare("SELECT path FROM textdb_search('\"fox 7\"', '/')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(none.is_empty());
    // Export the rest.
    let exported: i64 = conn.query_row("SELECT count(*) FROM textdb_export('/a')", [], |r| r.get(0)).unwrap();
    assert_eq!(exported, 13);
    // Re-create at a deleted path works.
    conn.execute("INSERT INTO kb(path, content) VALUES ('/b/moved/doc7.md', 'new')", []).unwrap();
}

#[test]
fn roundtrip_edge_cases_and_reimport() {
    let conn = setup();
    let db = TextDb::open(&conn, "kb_").unwrap();
    let cases: Vec<Vec<u8>> = vec![
        vec![],
        b"x".to_vec(),
        b"no trailing newline".to_vec(),
        b"crlf\r\nlines\r\n".to_vec(),
        vec![0xff, 0xfe, 0, 1, 2, b'\n', 0x80],
        (0..300_000u32).map(|i| (i % 251) as u8).collect(),
        "ąčę 一二三 😀\n".repeat(5000).into_bytes(),
    ];
    for (i, c) in cases.iter().enumerate() {
        let p = format!("/rt/{}.bin", i);
        db.create(&p, c, None, None).unwrap();
        assert_eq!(&db.read(&p).unwrap(), c, "case {}", i);
        // Re-import identical content: no new version.
        let r = db.upsert(&p, c, None).unwrap();
        assert_eq!(r.version, 1, "case {}", i);
        assert_eq!(db.history(&p).unwrap().len(), 1);
    }
    let commits: i64 = conn.query_row("SELECT count(*) FROM kb_commit WHERE version > 1", [], |r| r.get(0)).unwrap();
    assert_eq!(commits, 0);
    // Random edits against the reference copy.
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(3);
    let mut reference: Vec<u8> = (0..2000).map(|i| format!("line {} {}\n", i, "x".repeat(i % 50)).into_bytes()).flatten().collect();
    db.create("/rt/edit.md", &reference, None, None).unwrap();
    for _ in 0..200 {
        let a = rng.gen_range(0..=reference.len());
        let b = (a + rng.gen_range(0..64)).min(reference.len());
        let repl: Vec<u8> = (0..rng.gen_range(0..80)).map(|_| if rng.gen_bool(0.1) { b'\n' } else { b'y' }).collect();
        reference.splice(a..b, repl.iter().copied());
        db.update_content("/rt/edit.md", &reference, None, None, None).unwrap();
        assert_eq!(db.read("/rt/edit.md").unwrap(), reference);
    }
    let hist = db.history("/rt/edit.md").unwrap();
    assert!(hist.len() > 150);
    // Every historical version is materializable and the chunk store is shared.
    let (chunks, _nodes, commits, _files, chunk_bytes) = db.stats().unwrap();
    let v1 = db.read_version("/rt/edit.md", 1).unwrap();
    assert!(v1.starts_with(b"line 0 \nline 1 x\n"));
    eprintln!(
        "200 edits on {} bytes: {} commits, {} chunks, {} chunk bytes ({}x raw)",
        reference.len(),
        commits,
        chunks,
        chunk_bytes,
        chunk_bytes as f64 / reference.len() as f64
    );
    assert!((chunk_bytes as f64) < 8.0 * reference.len() as f64);
}

/// A bound on `path` is pushed into the shadow table so a subtree listing seeks through
/// `{p}node_path` instead of scanning every row. The bounds the cursor applies are widened
/// to `>=` / `<=` whatever the caller wrote, so the exact comparison has to come back from
/// SQLite — these cases are the ones that catch it if it does not.
#[test]
fn path_range_listing_is_exact() {
    let conn = setup();
    for p in [
        "/a.md",
        "/notes/a.md",
        "/notes/b.md",
        "/notes/sub/c.md",
        "/notes0/d.md", // sorts immediately after "/notes/…" and must never be included
        "/nz.md",
    ] {
        conn.execute("INSERT INTO kb(path, content) VALUES (?1, 'x')", params![p]).unwrap();
    }
    let paths = |sql: &str, args: &[&str]| -> Vec<String> {
        let mut st = conn.prepare(sql).unwrap();
        st.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };

    // The subtree of /notes, as the engine itself spells it.
    let under = paths(
        "SELECT path FROM kb WHERE path >= ?1 AND path < ?2 ORDER BY path",
        &["/notes/", "/notes0"],
    );
    assert_eq!(under, ["/notes/a.md", "/notes/b.md", "/notes/sub", "/notes/sub/c.md"]);

    // Strict lower bound: "/notes/a.md" itself must drop out.
    let strict = paths(
        "SELECT path FROM kb WHERE path > ?1 AND path < ?2 ORDER BY path",
        &["/notes/a.md", "/notes0"],
    );
    assert_eq!(strict, ["/notes/b.md", "/notes/sub", "/notes/sub/c.md"]);

    // Inclusive upper bound: "/notes/b.md" must be kept.
    let inclusive = paths(
        "SELECT path FROM kb WHERE path >= ?1 AND path <= ?2 ORDER BY path",
        &["/notes/", "/notes/b.md"],
    );
    assert_eq!(inclusive, ["/notes/a.md", "/notes/b.md"]);

    // One-sided bounds still work, and the root folder row stays hidden as in a full scan.
    let from = paths("SELECT path FROM kb WHERE path >= ?1 ORDER BY path", &["/notes0"]);
    assert_eq!(from, ["/notes0", "/notes0/d.md", "/nz.md"]);
    let upto = paths("SELECT path FROM kb WHERE path <= ?1 ORDER BY path", &["/a.md"]);
    assert_eq!(upto, ["/a.md"]);

    // A range and an equality on the same column: equality wins and is still exact.
    let eq: String = conn
        .query_row(
            "SELECT path FROM kb WHERE path = '/notes/b.md' AND path >= '/notes/' AND path < '/notes0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(eq, "/notes/b.md");

    // And the full scan is unchanged.
    let all = conn
        .prepare("SELECT path FROM kb ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        all,
        [
            "/a.md",
            "/notes",
            "/notes/a.md",
            "/notes/b.md",
            "/notes/sub",
            "/notes/sub/c.md",
            "/notes0",
            "/notes0/d.md",
            "/nz.md"
        ]
    );
}

/// The virtual table keeps a long-lived handle with a statement cache, so the statements it
/// caches are unfinalized for as long as the table exists. `sqlite3_close` tears virtual
/// tables down *before* it checks for unfinalized statements, so this is safe — but an
/// earlier attempt at the same optimisation was reverted for leaving the database file
/// unreleasable, and nothing caught it. This does.
#[test]
fn closing_a_connection_with_a_kb_table_releases_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kb.db");
    let conn = Connection::open(&file).unwrap();
    textdb_sqlite::register(&conn, "kb_").unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
    conn.execute("INSERT INTO kb(path, content) VALUES ('/a.md', 'x')", []).unwrap();
    // Read twice: the first read populates the statement cache, the second uses it. Closing
    // with a cold cache would not exercise anything.
    for _ in 0..2 {
        let s: String = conn
            .query_row("SELECT content FROM kb WHERE path = '/a.md'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s, "x");
    }
    // A range read too, so the other cached statement shape is live as well.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM kb WHERE path >= '/' AND path < '0'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    // The scalar and table-valued functions prepare through the *same* shared handle, so
    // they add statements to the cache the table's teardown is responsible for. Warm each
    // kind, or the close below would only be testing the table's own statements.
    for _ in 0..2 {
        let _: String = conn
            .query_row("SELECT textdb_content('/a.md', 1)", [], |r| r.get(0))
            .unwrap();
        let _: String = conn.query_row("SELECT textdb_lines('/a.md', 1, 1)", [], |r| r.get(0)).unwrap();
        let _: i64 = conn
            .query_row("SELECT count(*) FROM textdb_history('/a.md')", [], |r| r.get(0))
            .unwrap();
        let _: i64 = conn.query_row("SELECT count(*) FROM textdb_ls('/')", [], |r| r.get(0)).unwrap();
        let _: i64 = conn
            .query_row("SELECT count(*) FROM textdb_search('x', '/')", [], |r| r.get(0))
            .unwrap();
    }
    // A write through a scalar function too: those take a transaction on the shared handle.
    let _: i64 = conn
        .query_row("SELECT textdb_edit('/a.md', 'x', 'xy')", [], |r| r.get(0))
        .unwrap();

    // This is the assertion: `close` must succeed, not return SQLITE_BUSY.
    conn.close().expect("connection with a kb table must close cleanly");

    // And the file must be releasable afterwards — the symptom the reverted attempt had.
    std::fs::remove_file(&file).expect("database file must be deletable after close");
    assert!(!file.exists());

    // Reopening the same store still works, which catches a teardown that closed too much.
    let conn = Connection::open(dir.path().join("kb2.db")).unwrap();
    textdb_sqlite::register(&conn, "kb_").unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
    conn.execute("INSERT INTO kb(path, content) VALUES ('/b.md', 'y')", []).unwrap();
    let s: String = conn
        .query_row("SELECT content FROM kb WHERE path = '/b.md'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(s, "y");
    conn.close().unwrap();
}

/// `DROP TABLE` on a `textdb` table runs `xDestroy`, which drops the shadow tables through
/// the same long-lived handle the statement cache lives on.
#[test]
fn dropping_a_kb_table_removes_the_shadow_tables() {
    let conn = setup();
    conn.execute("INSERT INTO kb(path, content) VALUES ('/a.md', 'x')", []).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE name LIKE 'kb\\_%' ESCAPE '\\'", [], |r| r.get(0))
        .unwrap();
    assert!(before > 0, "shadow tables should exist");
    conn.execute_batch("DROP TABLE kb;").unwrap();
    let after: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE name LIKE 'kb\\_%' ESCAPE '\\'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, 0, "shadow tables should be gone after DROP TABLE");
}

/// Structure rows are HEAD-only and derived, so a commit that leaves the headings alone
/// rewrites nothing and only carries the `version` column forward. `textdb_section` looks
/// rows up at the file's *current* version, so a skipped rewrite that forgot to bump the
/// version would silently stop finding sections — this walks several edits to catch that.
#[test]
fn sections_stay_findable_across_edits_that_do_not_change_structure() {
    let conn = setup();
    let body = "---\ntitle: T\n---\n\n# One\n\nalpha\nbeta\n\n## Two\n\ngamma see [[other]]\n";
    conn.execute("INSERT INTO kb(path, content) VALUES ('/n/a.md', ?1)", params![body]).unwrap();

    let section = |heading: &str| -> Option<String> {
        conn.query_row("SELECT textdb_section('/n/a.md', ?1)", params![heading], |r| r.get(0))
            .optional()
            .unwrap()
            .flatten()
    };
    let counts = || -> (i64, i64, i64) {
        let q = |t: &str| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM kb_{} WHERE version = (SELECT version FROM kb WHERE path = '/n/a.md')", t), [], |r| r.get(0))
                .unwrap()
        };
        (q("section"), q("link"), q("frontmatter"))
    };
    assert_eq!(counts(), (2, 1, 1), "two headings, one wikilink, one frontmatter block");
    assert!(section("Two").unwrap().contains("gamma"));

    // Body-only edits: structure unchanged, so only the version is carried forward.
    for (old, new) in [("alpha", "ALPHA"), ("ALPHA", "alpha again"), ("beta", "BETA")] {
        conn.query_row("SELECT textdb_edit('/n/a.md', ?1, ?2)", params![old, new], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(counts(), (2, 1, 1), "rows must follow the new version after editing {}", old);
        assert!(section("Two").unwrap().contains("gamma"), "sections must stay findable after editing {}", old);
    }

    // A structural edit must actually rewrite the rows.
    conn.query_row("SELECT textdb_edit('/n/a.md', '## Two', '## Renamed')", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert_eq!(counts(), (2, 1, 1));
    assert!(section("Two").is_none(), "the old heading must be gone");
    assert!(section("Renamed").unwrap().contains("gamma"));

    // Adding a heading and a link changes the row counts.
    conn.query_row(
        "SELECT textdb_edit('/n/a.md', 'gamma see [[other]]', ?1)",
        params!["gamma see [[other]] and [[third]]\n\n### Deep\n\ndelta"],
        |r| r.get::<_, i64>(0),
    )
    .unwrap();
    assert_eq!(counts(), (3, 2, 1));
    assert!(section("Deep").unwrap().contains("delta"));

    // Removing the frontmatter clears its row rather than leaving a stale one behind.
    conn.query_row("SELECT textdb_edit('/n/a.md', ?1, '')", params!["---\ntitle: T\n---\n\n"], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap();
    let (secs, links, fm) = counts();
    assert_eq!((secs, links, fm), (3, 2, 0), "frontmatter row should be gone");
    assert!(section("Deep").unwrap().contains("delta"));
}

/// A connection's account does not outlive the connection.
///
/// `textdb_auth` leaves the view in a process-wide map keyed by the `sqlite3*` pointer, because the
/// functions build a fresh `TextDb` per call and have nowhere else to keep it. Nothing cleared that
/// map: `access::clear_session` existed and had no caller, so a closed connection left its account
/// behind and the next connection allocated at the same address answered as that account. Asserted
/// on the handle itself rather than by hoping the allocator reuses an address.
#[test]
fn a_connection_stops_being_an_account_when_it_closes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kb.db");
    let bearer = {
        let conn = textdb_sqlite::open(file.to_str().unwrap()).unwrap();
        let db = TextDb::open(&conn, "kb_").unwrap();
        db.ensure_folder("/sales").unwrap();
        db.create("/sales/a.md", b"alpha\n", None, None).unwrap();
        let node = db.entry("/sales").unwrap().id;
        drop(db);
        let now = "2026-01-01T00:00:00.000Z";
        let account = textdb_sqlite::access::create_account(&conn, "kb_", "agent", "agent", Some(node), now).unwrap();
        let (bearer, _) = textdb_sqlite::access::create_token(&conn, "kb_", account.id, None, None, now).unwrap();
        bearer
    };

    let handle = {
        let conn = textdb_sqlite::open(file.to_str().unwrap()).unwrap();
        let handle = unsafe { conn.handle() } as usize;
        let who: String = conn.query_row("SELECT textdb_auth(?1)", [&bearer], |r| r.get(0)).unwrap();
        assert_eq!(who, "agent");
        assert!(!textdb_sqlite::access::session(handle).is_admin(), "the connection is the account while it is open");
        handle
    };
    // Closed: whatever is allocated at that address next is the owner, as any fresh connection is.
    assert!(textdb_sqlite::access::session(handle).is_admin(), "a closed connection must not leave its account behind");
}

/// The scalar functions borrow a handle a `textdb` table registered. With no table on the
/// connection there is nothing to borrow, and they must still work on their own — this is
/// the fallback path in `with_db`, which nothing else exercises.
#[test]
fn scalar_functions_work_without_a_virtual_table() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("kb.db");
    let conn = Connection::open(&file).unwrap();
    // The shadow tables through the Rust API, so no virtual table is ever created and
    // nothing registers the handle; then the functions on their own.
    let db = TextDb::open(&conn, "kb_").unwrap();
    db.create("/a.md", b"alpha\nbeta\n", None, None).unwrap();
    drop(db);
    textdb_sqlite::functions::register_functions(&conn, "kb_").unwrap();
    let content: String = conn
        .query_row("SELECT textdb_content('/a.md')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(content, "alpha\nbeta\n");
    let lines: String = conn.query_row("SELECT textdb_lines('/a.md', 2, 2)", [], |r| r.get(0)).unwrap();
    assert_eq!(lines, "beta\n");
    let v: i64 = conn
        .query_row("SELECT textdb_edit('/a.md', 'beta', 'BETA')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
    conn.close().unwrap();
    std::fs::remove_file(&file).expect("file must be deletable with no table registered either");
}

/// A vault with a repeated heading, a nested tree, and a document that shares no heading, so
/// scope, matching and the level filter can each be told apart from the others.
fn outline_vault() -> Connection {
    let conn = setup();
    let docs = [
        (
            "/notes/a.md",
            "---\ntitle: A\n---\n# Alpha\nintro words here\n\n## Goals\nwe want things\n\n### Detail\nfine print\n\n## Next Steps\nship it\n",
        ),
        ("/notes/b.md", "# Beta\nbody\n\n## next steps\nlater\n"),
        ("/other/c.md", "# Gamma\nonly words\n"),
    ];
    for (path, body) in docs {
        conn.execute("INSERT INTO kb(path, content) VALUES (?1, ?2)", params![path, body]).unwrap();
    }
    conn
}

fn headings(conn: &Connection, sql: &str) -> Vec<String> {
    let mut st = conn.prepare(sql).unwrap();
    let rows = st
        .query_map([], |r| Ok(format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

#[test]
fn outline_scopes_to_a_document_a_folder_or_the_vault() {
    let conn = outline_vault();
    assert_eq!(
        headings(&conn, "SELECT path, heading FROM textdb_outline('/notes/a.md')"),
        ["/notes/a.md:Alpha", "/notes/a.md:Goals", "/notes/a.md:Detail", "/notes/a.md:Next Steps"]
    );
    // A folder takes everything below it and nothing beside it.
    assert_eq!(
        headings(&conn, "SELECT path, heading FROM textdb_outline('/other')"),
        ["/other/c.md:Gamma"]
    );
    // The vault is every document, in path order, and each document in line order.
    let all = headings(&conn, "SELECT path, heading FROM textdb_outline('/')");
    assert_eq!(all.len(), 7);
    assert_eq!(all[0], "/notes/a.md:Alpha");
    assert_eq!(all[6], "/other/c.md:Gamma");
}

#[test]
fn outline_matches_headings_folded_and_by_shape() {
    let conn = outline_vault();
    // Exact, ignoring case: "Next Steps" and "next steps" are the same heading.
    assert_eq!(
        headings(&conn, "SELECT path, heading FROM textdb_outline('/', 'NEXT STEPS')"),
        ["/notes/a.md:Next Steps", "/notes/b.md:next steps"]
    );
    assert_eq!(
        headings(&conn, "SELECT path, heading FROM textdb_outline('/', 'next', 'prefix')"),
        ["/notes/a.md:Next Steps", "/notes/b.md:next steps"]
    );
    assert_eq!(
        headings(&conn, "SELECT path, heading FROM textdb_outline('/', 'tep', 'contains')"),
        ["/notes/a.md:Next Steps", "/notes/b.md:next steps"]
    );
    // A prefix that matches nothing is empty rather than everything.
    assert!(headings(&conn, "SELECT path, heading FROM textdb_outline('/', 'zzz', 'prefix')").is_empty());
    // `%` and `_` in a contains pattern mean themselves, not wildcards.
    assert!(headings(&conn, "SELECT path, heading FROM textdb_outline('/', 'n%t', 'contains')").is_empty());
}

#[test]
fn outline_caps_depth_and_carries_file_metadata() {
    let conn = outline_vault();
    assert_eq!(
        headings(&conn, "SELECT path, heading FROM textdb_outline('/', NULL, 'exact', 1)"),
        ["/notes/a.md:Alpha", "/notes/b.md:Beta", "/other/c.md:Gamma"]
    );
    // Every row carries its document's own figures, so a table needs no query per row.
    let (nbytes, nlines, file_words, version): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT nbytes, nlines, file_nwords, version FROM textdb_outline('/notes/a.md') LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    let (real_bytes, real_lines): (i64, i64) = conn
        .query_row("SELECT nbytes, nlines FROM kb WHERE path = '/notes/a.md'", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((nbytes, nlines, version), (real_bytes, real_lines, 1));
    assert!(file_words > 0);
}

#[test]
fn section_word_counts_compose_and_follow_edits() {
    let conn = outline_vault();
    let counts = |conn: &Connection| -> Vec<(String, i64, i64)> {
        let mut st = conn
            .prepare("SELECT heading, nwords, nwords_total FROM textdb_outline('/notes/a.md')")
            .unwrap();
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    };
    let before = counts(&conn);
    // Alpha's total is its own words plus every nested section's own words.
    let nested: i64 = before[1..].iter().map(|(_, own, _)| own).sum();
    assert_eq!(before[0].2, before[0].1 + nested);
    // A leaf's two figures agree.
    assert_eq!(before[2].1, before[2].2);

    // A body edit that leaves every heading where it is still refreshes the counts.
    conn.execute(
        "UPDATE kb SET content = replace(content, 'ship it', 'ship it and then celebrate') WHERE path = '/notes/a.md'",
        [],
    )
    .unwrap();
    let after = counts(&conn);
    assert_eq!(after[3].1, before[3].1 + 3, "Next Steps gained three words");
    assert_eq!(after[0].2, before[0].2 + 3, "and so did the root's total");
    // The heading tree itself did not move.
    let names: Vec<_> = after.iter().map(|(h, _, _)| h.clone()).collect();
    assert_eq!(names, before.iter().map(|(h, _, _)| h.clone()).collect::<Vec<_>>());
}

#[test]
fn heading_names_counts_sections_and_documents() {
    let conn = outline_vault();
    let mut st = conn.prepare("SELECT heading, sections, docs FROM textdb_headings('/', 'ne')").unwrap();
    let rows: Vec<(String, i64, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    // The two spellings fold to one entry, counted across both documents.
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].1, rows[0].2), (2, 2));
    // Folder scope narrows the same call.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM textdb_headings('/other', '')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn word_count_has_history_the_way_bytes_and_lines_do() {
    let conn = setup();
    conn.execute("INSERT INTO kb(path, content) VALUES ('/a.md', 'one two three\n')", []).unwrap();
    conn.execute("UPDATE kb SET content = 'one two three four five\n' WHERE path = '/a.md'", []).unwrap();
    let mut st = conn.prepare("SELECT version, nlines, nwords FROM textdb_history('/a.md')").unwrap();
    let rows: Vec<(i64, i64, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows, [(1, 1, 3), (2, 1, 5)]);
}

#[test]
fn a_deleted_document_leaves_the_outline() {
    let conn = outline_vault();
    conn.execute("DELETE FROM kb WHERE path = '/notes/b.md'", []).unwrap();
    let all = headings(&conn, "SELECT path, heading FROM textdb_outline('/')");
    assert!(all.iter().all(|h| !h.starts_with("/notes/b.md")), "{all:?}");
    // And its heading no longer counts towards the shared one.
    let docs: i64 = conn
        .query_row("SELECT docs FROM textdb_headings('/', 'next')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(docs, 1);
}

#[test]
fn a_hit_found_through_folding_shows_the_line_that_matched() {
    // The index tokenises with fts5's `unicode61`, which strips diacritics, so `facade`
    // finds a document holding `façade`. Choosing the line to show used to compare raw text,
    // find nothing and fall back to the top of the chunk: the document was right and the
    // line was wrong. See `textdb_core::fold`.
    let conn = setup();
    conn.execute(
        "INSERT INTO kb(path, content) VALUES ('/a.md', ?1)",
        params!["# Doc\n\nfiller line one\nthe word façade appears on this line\nfiller two\n"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kb(path, content) VALUES ('/b.md', ?1)",
        params!["# Kita\n\nnieko\nčia ąžuolas auga\n"],
    )
    .unwrap();

    for (query, want_line, want_in_snippet) in [
        ("facade", 4, "façade"),
        ("façade", 4, "façade"),
        ("azuolas", 4, "ąžuolas"),
        ("ąžuolas", 4, "ąžuolas"),
    ] {
        let (line, snippet): (i64, String) = conn
            .query_row(
                "SELECT line, text FROM textdb_search(?1, '/', 10)",
                params![query],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(line, want_line, "{query} showed the wrong line: {snippet:?}");
        assert!(snippet.contains(want_in_snippet), "{query} showed {snippet:?}");
    }
}

#[test]
fn a_snippet_shows_the_match_however_the_query_was_written() {
    let conn = setup();
    let pad = "filler word here ".repeat(30);
    conn.execute(
        "INSERT INTO kb(path, content) VALUES ('/a.md', ?1)",
        params![format!("---\ntitle: T\ndraft: false\n---\n# Head\n{pad}NEEDLE appears late{pad}\n")],
    )
    .unwrap();

    let hit = |q: &str| -> (i64, String) {
        conn.query_row("SELECT line, text FROM textdb_search(?1, '/', 5)", params![q], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap_or_else(|e| panic!("{q}: {e}"))
    };

    // A match past the cut length of a long line is still shown: the text is a window around
    // it, not the head of the line, and says so at the end it cut.
    let (line, snippet) = hit("needle");
    assert_eq!(line, 6);
    assert!(snippet.contains("NEEDLE appears late"), "{snippet:?}");
    assert!(snippet.starts_with('…') && snippet.ends_with('…'), "{snippet:?}");

    // A quoted phrase is matched word by word, so punctuation between the words does not
    // hide it: `query_terms` keeps the phrase as one term with a space in it.
    let (line, snippet) = hit("\"draft false\"");
    assert_eq!(line, 3);
    assert_eq!(snippet, "draft: false");

    // A line that fits is shown whole, with no ellipsis.
    let (_, snippet) = hit("head");
    assert_eq!(snippet, "# Head");
}

#[test]
fn a_heading_dense_document_stays_under_the_parameter_limit() {
    // SQLite allows 32,766 bound parameters per statement. The section insert batches rows,
    // and the bound is on *parameters*: 4,000 rows fitted at six columns per row and did not
    // at ten, so the batch size is derived from the column count rather than written down.
    let conn = setup();
    let mut body = String::from("---\ntitle: Big\n---\n");
    for i in 0..5000 {
        body.push_str(&format!("# Heading {i}\n\nbody line {i}\n\n"));
    }
    conn.execute("INSERT INTO kb(path, content) VALUES ('/big.md', ?1)", params![body]).unwrap();
    let n: i64 = conn
        .query_row("SELECT nsections FROM textdb_entry('/big.md')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 5000);
    // And an edit that leaves the heading tree alone takes the counts-only path, whose batch
    // has the same bound.
    conn.execute("UPDATE kb SET content = replace(content, 'body line 0', 'body line zero') WHERE path = '/big.md'", [])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM textdb_outline('/big.md', NULL, 'exact', NULL, 100000)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 5000);
}

/// `textdb_links` and `textdb_backlinks` answer with the one canonical row, in both directions.
#[test]
fn links_and_backlinks_are_table_valued_functions() {
    let conn = setup();
    let put = |path: &str, body: &str| {
        conn.execute("INSERT INTO kb(path, content) VALUES (?1, ?2)", params![path, body]).unwrap();
    };
    put("/g/index.md", "# Guide\n\nSee [the limits page](limits.md) and [[Missing]].\n");
    put("/g/limits.md", "# Limits\n");

    let cols: Vec<String> = conn
        .prepare("SELECT * FROM textdb_links('/g')")
        .unwrap()
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(cols, ["path", "version", "line", "kind", "target", "anchor", "alias", "status", "resolved", "asset"]);

    let rows: Vec<(String, i64, i64, String, String, Option<String>, Option<String>)> = conn
        .prepare("SELECT path, version, line, kind, target, alias, status FROM textdb_links('/g')")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            ("/g/index.md".into(), 1, 3, "md".into(), "limits.md".into(), Some("the limits page".into()), Some("ok".into())),
            ("/g/index.md".into(), 1, 3, "wiki".into(), "Missing".into(), None, Some("broken".into())),
        ]
    );

    let broken: Vec<String> = conn
        .prepare("SELECT target FROM textdb_links('/g', 'broken')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(broken, ["Missing"]);

    let back: Vec<(String, i64)> = conn
        .prepare("SELECT path, line FROM textdb_backlinks('/g/limits.md')")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(back, [("/g/index.md".to_string(), 3)]);

    // A status the store never writes is a mistake, not an empty result. The vtab filters on
    // the first row, so the error arrives when the rows are drawn rather than at prepare time.
    let bad = conn
        .prepare("SELECT * FROM textdb_links('/g', 'nope')")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>();
    assert!(bad.is_err(), "{bad:?}");
}

/// A search scoped to a folder must find what is in it, however large the store around it.
///
/// The FTS index is over chunks and the ranker has no early termination, so the retrieval used to
/// take the best `limit * 50` chunks *globally* and apply the folder and visibility filters to
/// what was left. Past that many matches elsewhere, a scoped search returned nothing at all: a
/// five-document folder whose every document held the word answered empty, and so did an account
/// whose entire vault was that folder. Wrong answers, arriving only once a store got big.
///
/// 3,000 documents is comfortably past `10 * 50` and small enough to stay a unit test.
#[test]
fn a_scoped_search_is_not_truncated_by_the_rest_of_the_store() {
    let conn = setup();
    let db = TextDb::attach(&conn, "kb_", false);
    for i in 0..3_000 {
        db.create(&format!("/bulk/{}/{i}.md", i / 500), format!("common filler text, bulk {i}\n").as_bytes(), None, None)
            .unwrap();
    }
    for i in 0..5 {
        db.create(&format!("/tiny/{i}.md"), format!("common filler text, tiny {i}\n").as_bytes(), None, None).unwrap();
    }

    // Unscoped, the store answers with whatever ranks best; that has always worked.
    assert_eq!(db.search_lines("common", "/", 10, 1).unwrap().len(), 10, "unscoped");

    // Scoped to the small folder, every one of its documents matches and all five must come back.
    let hits = db.search_lines("common", "/tiny", 10, 1).unwrap();
    assert_eq!(hits.len(), 5, "a folder of five matching documents inside a store of 3,000");
    assert!(hits.iter().all(|h| h.path.starts_with("/tiny/")), "{:?}", hits.iter().map(|h| &h.path).collect::<Vec<_>>());
}
