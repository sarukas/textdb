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
            "SELECT content, version, kind, parent_path FROM kb WHERE path = '/notes/a.md'",
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
        .prepare("SELECT path, line, snippet FROM textdb_search('quick fox', '/a/sub1')")
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
