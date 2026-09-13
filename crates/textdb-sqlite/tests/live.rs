//! What a live client needs from the store: the change feed, line hunks between versions,
//! chunk listings, line-range edits against a stale read, and upgrading an older store.

use rusqlite::Connection;
use textdb_core::TextdbError;
use textdb_sqlite::{open_in_memory, TextDb};

fn setup() -> Connection {
    let conn = open_in_memory().unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
    conn
}

type FeedRow = (i64, String, String, Option<String>, Option<i64>, Option<i64>, Option<String>, Option<String>);

fn feed(conn: &Connection, since: i64) -> Vec<FeedRow> {
    conn.prepare("SELECT seq, op, path, old_path, version, base_version, commit_kind, author FROM textdb_feed(?1)")
        .unwrap()
        .query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn feed_records_every_namespace_and_content_change() {
    let conn = setup();
    conn.execute(
        "INSERT INTO kb(path, content, author) VALUES ('/notes/a.md', ?1, 'alice')",
        ["# A\n\none\ntwo\n"],
    )
    .unwrap();
    conn.query_row("SELECT textdb_edit('/notes/a.md', 'two', 'TWO', 'agent')", [], |_| Ok(()))
        .unwrap();
    conn.execute("UPDATE kb SET path = '/archive/a.md', author = 'bob' WHERE path = '/notes/a.md'", [])
        .unwrap();
    conn.execute("DELETE FROM kb WHERE path = '/archive'", []).unwrap();

    let rows = feed(&conn, 0);
    let ops: Vec<(&str, &str)> = rows.iter().map(|r| (r.1.as_str(), r.2.as_str())).collect();
    assert_eq!(
        ops,
        vec![
            ("mkdir", "/notes"),
            ("create", "/notes/a.md"),
            ("commit", "/notes/a.md"),
            ("mkdir", "/archive"),
            ("move", "/archive/a.md"),
            ("delete", "/archive"),
        ]
    );
    assert!(rows.windows(2).all(|w| w[0].0 < w[1].0), "seq must increase: {rows:?}");
    let create = &rows[1];
    assert_eq!((create.4, create.5, create.6.as_deref(), create.7.as_deref()), (Some(1), None, Some("direct"), Some("alice")));
    let commit = &rows[2];
    assert_eq!((commit.4, commit.5, commit.6.as_deref(), commit.7.as_deref()), (Some(2), Some(1), Some("direct"), Some("agent")));
    let moved = &rows[4];
    assert_eq!((moved.3.as_deref(), moved.7.as_deref()), (Some("/notes/a.md"), Some("bob")));
    // Reading from a sequence number returns only what came after it.
    assert_eq!(feed(&conn, commit.0).len(), 3);
    let last: i64 = conn.query_row("SELECT textdb_last_seq()", [], |r| r.get(0)).unwrap();
    assert_eq!(last, rows.last().unwrap().0);
}

type HunkRow = (i64, i64, i64, i64, String, String);

fn hunks(conn: &Connection, sql: &str) -> Vec<HunkRow> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn hunks_patch_one_version_into_the_next() {
    let conn = setup();
    let body: String = (0..400)
        .map(|i| format!("line {i} of a document long enough to span many chunks\n"))
        .collect();
    conn.execute("INSERT INTO kb(path, content) VALUES ('/big.md', ?1)", [&body]).unwrap();
    let edited = body
        .replace("line 10 of", "LINE TEN of")
        .replace("line 300 of a document long enough to span many chunks\n", "");
    let edited = format!("{edited}appended\n");
    conn.execute("UPDATE kb SET content = ?1 WHERE path = '/big.md'", [&edited]).unwrap();

    let cols = "old_from, old_count, new_from, new_count, old_text, new_text";
    let latest = hunks(&conn, &format!("SELECT {cols} FROM textdb_hunks('/big.md')"));
    assert_eq!(latest.len(), 3, "{latest:?}");
    assert_eq!(
        latest[0],
        (
            11,
            1,
            11,
            1,
            "line 10 of a document long enough to span many chunks\n".to_string(),
            "LINE TEN of a document long enough to span many chunks\n".to_string()
        )
    );
    assert_eq!((latest[1].0, latest[1].1, latest[1].3), (301, 1, 0));
    assert_eq!((latest[2].0, latest[2].1, latest[2].2, latest[2].5.as_str()), (401, 0, 400, "appended\n"));
    // Explicit versions give the same answer as the defaults.
    assert_eq!(hunks(&conn, &format!("SELECT {cols} FROM textdb_hunks('/big.md', 1, 2)")), latest);
    assert_eq!(hunks(&conn, &format!("SELECT {cols} FROM textdb_hunks('/big.md', 1)")), latest);

    // Applying them to version 1, last first so earlier line numbers stay valid, gives version 2.
    let mut lines: Vec<String> = body.split_inclusive('\n').map(String::from).collect();
    for h in latest.iter().rev() {
        let at = (h.0 - 1) as usize;
        lines.splice(at..at + h.1 as usize, h.5.split_inclusive('\n').map(String::from));
    }
    assert_eq!(lines.concat(), edited);

    // Version 0 is the empty document, so the first commit is one whole-document insertion.
    let first = hunks(&conn, &format!("SELECT {cols} FROM textdb_hunks('/big.md', 0, 1)"));
    assert_eq!(first, vec![(1, 0, 1, 400, String::new(), body.clone())]);
}

#[test]
fn chunks_tile_the_document_and_keep_their_hashes_across_versions() {
    let conn = setup();
    let body: String = (0..2000).map(|i| format!("row {i} filler filler filler\n")).collect();
    conn.execute("INSERT INTO kb(path, content) VALUES ('/c.md', ?1)", [&body]).unwrap();
    let listing = |sql: &str| -> Vec<(i64, String, i64, i64, i64, i64)> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let v1 = listing("SELECT ord, hash, byte_from, nbytes, line_from, nlines FROM textdb_chunks('/c.md')");
    assert!(v1.len() > 10, "expected many chunks, got {}", v1.len());
    let (mut byte, mut line) = (0i64, 1i64);
    for (i, c) in v1.iter().enumerate() {
        assert_eq!((c.0, c.2, c.4), (i as i64, byte, line));
        assert_eq!(c.1.len(), 64);
        byte += c.3;
        line += c.5;
    }
    assert_eq!((byte as usize, line - 1), (body.len(), 2000));

    conn.execute("UPDATE kb SET content = replace(content, 'row 1990 ', 'ROW 1990 ') WHERE path = '/c.md'", [])
        .unwrap();
    let v2: Vec<String> = listing("SELECT ord, hash, byte_from, nbytes, line_from, nlines FROM textdb_chunks('/c.md')")
        .into_iter()
        .map(|c| c.1)
        .collect();
    let v1_again: Vec<String> = listing("SELECT ord, hash, byte_from, nbytes, line_from, nlines FROM textdb_chunks('/c.md', 1)")
        .into_iter()
        .map(|c| c.1)
        .collect();
    assert_eq!(v1_again, v1.iter().map(|c| c.1.clone()).collect::<Vec<_>>());
    // An edit near the end leaves every chunk before it untouched.
    let shared = v2.iter().zip(&v1_again).take_while(|(a, b)| a == b).count();
    assert!(shared + 3 >= v1_again.len(), "only {shared} of {} leading chunks survived", v1_again.len());
}

#[test]
fn replace_lines_against_a_stale_read_is_rebased() {
    let conn = setup();
    let db = TextDb::attach(&conn, "kb_", true);
    let body: String = (0..2000).map(|i| format!("line {i} with some filler text\n")).collect();
    db.create("/r.md", body.as_bytes(), Some("human"), None).unwrap();
    // An agent reads version 1; meanwhile a human edits far down the file.
    db.edit("/r.md", b"line 1500 with", b"line 1500 (human) with", Some("human")).unwrap();
    // The agent's line numbers still refer to version 1.
    let r = db.replace_lines("/r.md", 3, 4, b"three\nfour\n", Some(1), Some("agent")).unwrap();
    assert_eq!((r.version, r.kind.as_str()), (3, "rebased"));
    let text = String::from_utf8(db.read("/r.md").unwrap()).unwrap();
    assert!(text.starts_with("line 0 with some filler text\nline 1 with some filler text\nthree\nfour\nline 4 with"));
    assert!(text.contains("line 1500 (human) with"));

    // Insert in front of line 1.
    db.replace_lines("/r.md", 1, 0, b"# top\n", None, None).unwrap();
    assert!(String::from_utf8(db.read("/r.md").unwrap()).unwrap().starts_with("# top\nline 0 with"));
    // Append after an unterminated last line: the newline it lacks is supplied.
    db.create("/u.md", b"a\nb", None, None).unwrap();
    db.replace_lines("/u.md", 3, 2, b"c\n", None, None).unwrap();
    assert_eq!(db.read("/u.md").unwrap(), b"a\nb\nc\n");
    // Replace the last line.
    db.replace_lines("/u.md", 3, 3, b"C\n", None, None).unwrap();
    assert_eq!(db.read("/u.md").unwrap(), b"a\nb\nC\n");
    assert!(matches!(db.replace_lines("/u.md", 9, 9, b"x", None, None), Err(TextdbError::InvalidEdit(_))));
    assert!(matches!(db.replace_lines("/u.md", 3, 1, b"x", None, None), Err(TextdbError::InvalidEdit(_))));

    // History records how each commit landed and which version it started from.
    let hist: Vec<(i64, Option<String>, Option<i64>)> = conn
        .prepare("SELECT version, kind, base_version FROM textdb_history('/r.md')")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        hist,
        vec![
            (1, Some("direct".into()), None),
            (2, Some("direct".into()), Some(1)),
            (3, Some("rebased".into()), Some(1)),
            (4, Some("direct".into()), Some(3)),
        ]
    );
}

#[test]
fn scalar_write_and_replace_lines_report_how_the_commit_landed() {
    let conn = setup();
    let q = |sql: &str| -> serde_json::Value {
        serde_json::from_str(&conn.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap()).unwrap()
    };
    assert_eq!(q("SELECT textdb_write('/s.md', 'one' || char(10) || 'two' || char(10))"), serde_json::json!({"version": 1, "kind": "direct"}));
    assert_eq!(
        q("SELECT textdb_write('/s.md', 'one' || char(10) || 'two' || char(10), NULL, 'me')"),
        serde_json::json!({"version": 1, "kind": "noop"})
    );
    assert_eq!(
        q("SELECT textdb_replace_lines('/s.md', 2, 2, 'TWO' || char(10), 1, 'agent')"),
        serde_json::json!({"version": 2, "kind": "direct"})
    );
    let content: String = conn.query_row("SELECT textdb_content('/s.md')", [], |r| r.get(0)).unwrap();
    assert_eq!(content, "one\nTWO\n");
    // Drivers that bind every number as a double (node:sqlite) still address lines and versions,
    // but a fractional line number is refused rather than truncated.
    let line: String = conn.query_row("SELECT textdb_lines('/s.md', 2.0, 2.0)", [], |r| r.get(0)).unwrap();
    assert_eq!(line, "TWO\n");
    let old: String = conn.query_row("SELECT textdb_content('/s.md', 1.0)", [], |r| r.get(0)).unwrap();
    assert_eq!(old, "one\ntwo\n");
    assert!(conn.query_row("SELECT textdb_lines('/s.md', 1.5, 2)", [], |r| r.get::<_, String>(0)).is_err());
    // An empty author is no author.
    conn.query_row("SELECT textdb_append('/s.md', 'three' || char(10), '')", [], |_| Ok(())).unwrap();
    let author: Option<String> =
        conn.query_row("SELECT author FROM textdb_history('/s.md') ORDER BY version DESC LIMIT 1", [], |r| r.get(0)).unwrap();
    assert_eq!(author, None);
}

#[test]
fn an_older_store_is_upgraded_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    let path = path.to_str().unwrap();
    {
        let conn = textdb_sqlite::open(path).unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
        conn.execute("INSERT INTO kb(path, content) VALUES ('/a.md', 'one')", []).unwrap();
        // Take the store back to the schema this build replaced.
        conn.execute_batch(
            "DROP TABLE kb_change; ALTER TABLE kb_commit DROP COLUMN kind; ALTER TABLE kb_commit DROP COLUMN base_version;",
        )
        .unwrap();
    }
    let conn = textdb_sqlite::open(path).unwrap();
    let added: i64 = conn.query_row("SELECT textdb_migrate()", [], |r| r.get(0)).unwrap();
    assert_eq!(added, 2);
    let again: i64 = conn.query_row("SELECT textdb_migrate()", [], |r| r.get(0)).unwrap();
    assert_eq!(again, 0);
    conn.execute("UPDATE kb SET content = 'two' WHERE path = '/a.md'", []).unwrap();
    let (v, kind): (i64, Option<String>) = conn
        .query_row(
            "SELECT version, kind FROM textdb_history('/a.md') ORDER BY version DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((v, kind.as_deref()), (2, Some("direct")));
    let n: i64 = conn.query_row("SELECT count(*) FROM textdb_feed(0)", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn another_connection_sees_commits_through_data_version_and_the_feed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.db");
    let path = path.to_str().unwrap();
    let writer = textdb_sqlite::open(path).unwrap();
    writer.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
    let watcher = textdb_sqlite::open(path).unwrap();
    watcher
        .execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');")
        .unwrap();
    let data_version = |c: &Connection| -> i64 { c.query_row("PRAGMA data_version", [], |r| r.get(0)).unwrap() };
    let before = data_version(&watcher);
    let since: i64 = watcher.query_row("SELECT textdb_last_seq()", [], |r| r.get(0)).unwrap();

    writer
        .execute("INSERT INTO kb(path, content, author) VALUES ('/w.md', 'hello', 'agent')", [])
        .unwrap();

    assert_ne!(data_version(&watcher), before, "data_version must move when another connection commits");
    let seen: Vec<(String, String, Option<String>)> = watcher
        .prepare("SELECT op, path, author FROM textdb_feed(?1)")
        .unwrap()
        .query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(seen, vec![("create".into(), "/w.md".into(), Some("agent".into()))]);
}
