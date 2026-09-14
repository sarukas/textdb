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

#[test]
fn scalar_move_and_delete_act_on_whole_subtrees_with_an_author() {
    let conn = setup();
    for p in ["/f/a.md", "/f/sub/b.md", "/f/sub/deep/c.md"] {
        conn.query_row("SELECT textdb_write(?1, 'x')", [p], |_| Ok(())).unwrap();
    }
    let since: i64 = conn.query_row("SELECT textdb_last_seq()", [], |r| r.get(0)).unwrap();
    let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0));

    assert_eq!(one("SELECT textdb_move('/f/sub', '/g/moved', 'human')").unwrap(), 1);
    assert_eq!(one("SELECT count(*) FROM kb WHERE path LIKE '/g/moved/%'").unwrap(), 3);
    assert_eq!(one("SELECT textdb_move('/f/a.md', '/f/renamed.md', '')").unwrap(), 1);
    let err = one("SELECT textdb_move('/g', '/g/inside')").unwrap_err().to_string();
    assert!(err.starts_with("TX004"), "{err}");
    let err = one("SELECT textdb_move('/nope.md', '/x.md')").unwrap_err().to_string();
    assert!(err.starts_with("TX003"), "{err}");

    assert_eq!(one("SELECT textdb_delete('/g', 'agent-7')").unwrap(), 1);
    assert_eq!(one("SELECT count(*) FROM kb WHERE path LIKE '/g%'").unwrap(), 0);
    let err = one("SELECT textdb_delete('/')").unwrap_err().to_string();
    assert!(err.starts_with("TX004"), "{err}");

    let ops: Vec<_> = feed(&conn, since)
        .into_iter()
        .filter(|r| r.1 == "move" || r.1 == "delete")
        .map(|r| (r.1, r.2, r.3, r.7))
        .collect();
    let s = |v: &str| v.to_string();
    assert_eq!(
        ops,
        vec![
            (s("move"), s("/g/moved"), Some(s("/f/sub")), Some(s("human"))),
            (s("move"), s("/f/renamed.md"), Some(s("/f/a.md")), None),
            (s("delete"), s("/g"), None, Some(s("agent-7"))),
        ]
    );
}

#[test]
fn trash_lists_reads_and_purges_what_deletes_left() {
    let conn = setup();
    let json = |sql: &str| -> serde_json::Value {
        serde_json::from_str(&conn.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap()).unwrap()
    };
    let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    let text = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, String>(0));
    let write = |path: &str, content: &str| {
        conn.query_row("SELECT textdb_write(?1, ?2)", [path, content], |_| Ok(())).unwrap();
    };
    // Long enough to span many chunks, so a file that starts the same way shares all but
    // its last chunk with it.
    let shared: String = (0..3000).map(|i| format!("shared paragraph {i} that both files contain\n")).collect();
    write("/keep.md", &shared);
    write("/old/a.md", &format!("{shared}alpha ending\n"));
    write("/old/a.md", "# a, second version\nbeta words\n");
    write("/old/sub/b.md", "zeta words\n");
    write("/lone.md", "lone words\n");
    conn.query_row("SELECT textdb_delete('/lone.md', 'agent-7')", [], |_| Ok(())).unwrap();
    conn.query_row("SELECT textdb_delete('/old', 'human')", [], |_| Ok(())).unwrap();

    let items = json("SELECT textdb_trash()");
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 2);
    let old = &items[0];
    assert_eq!(
        (old["path"].as_str(), old["kind"].as_str(), old["files"].as_i64(), old["deleted_by"].as_str()),
        (Some("/old"), Some("folder"), Some(2), Some("human"))
    );
    assert_eq!(items[1]["path"], "/lone.md");

    let inside = json(&format!("SELECT textdb_trash({})", old["id"]));
    let names: Vec<_> = inside.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["sub", "a.md"]);
    let a = inside[1]["id"].as_i64().unwrap();
    assert_eq!(inside[1]["deleted_by"], "human");
    assert_eq!(text(&format!("SELECT textdb_trash_content({a})")).unwrap(), "# a, second version\nbeta words\n");
    assert!(text(&format!("SELECT textdb_trash_content({a}, 1)")).unwrap().ends_with("alpha ending\n"));
    assert_eq!(json(&format!("SELECT textdb_trash_history({a})")).as_array().unwrap().len(), 2);
    assert_eq!(json(&format!("SELECT textdb_trash_entry({a})"))["path"], "/old/a.md");
    let err = text("SELECT textdb_trash_content(999999)").unwrap_err().to_string();
    assert!(err.starts_with("TX003"), "{err}");

    let chunks_before = one("SELECT count(*) FROM kb_chunk");
    let nodes_before = one("SELECT count(*) FROM kb_tree_node");
    let stats = json(&format!("SELECT textdb_purge({}, 'human')", old["id"]));
    assert_eq!(
        (stats["items"].as_i64(), stats["files"].as_i64(), stats["folders"].as_i64(), stats["versions"].as_i64()),
        (Some(1), Some(2), Some(2), Some(3))
    );
    let freed = stats["chunks"].as_i64().unwrap();
    assert!(freed > 0);
    assert_eq!(one("SELECT count(*) FROM kb_chunk"), chunks_before - freed);
    assert_eq!(one("SELECT count(*) FROM kb_tree_node"), nodes_before - stats["tree_nodes"].as_i64().unwrap());
    // The chunks /old/a.md shared with /keep.md are still there, and still searchable.
    assert_eq!(
        one("SELECT count(*) FROM textdb_chunks('/keep.md') c JOIN kb_chunk k ON lower(hex(k.hash)) = lower(c.hash)"),
        one("SELECT count(*) FROM textdb_chunks('/keep.md')")
    );
    assert_eq!(text("SELECT textdb_content('/keep.md')").unwrap(), shared);
    assert!(one("SELECT count(*) FROM kb_fts WHERE kb_fts MATCH 'paragraph'") > 0);
    // Content only the purged files had is gone from the index too.
    assert_eq!(one("SELECT count(*) FROM kb_fts WHERE kb_fts MATCH 'zeta'"), 0);
    assert_eq!(one("SELECT count(*) FROM kb_fts WHERE kb_fts MATCH 'beta'"), 0);
    let err = text(&format!("SELECT textdb_trash_content({a})")).unwrap_err().to_string();
    assert!(err.starts_with("TX003"), "{err}");
    assert_eq!(json("SELECT textdb_trash()").as_array().unwrap().len(), 1);

    // The path is free again.
    write("/old/a.md", "a new file\n");
    assert_eq!(one("SELECT version FROM kb WHERE path = '/old/a.md'"), 1);

    assert_eq!(one("SELECT count(*) FROM kb_fts WHERE kb_fts MATCH 'lone'"), 1);
    let stats = json("SELECT textdb_empty_trash('ops')");
    assert_eq!((stats["items"].as_i64(), stats["files"].as_i64()), (Some(1), Some(1)));
    assert_eq!(json("SELECT textdb_trash()").as_array().unwrap().len(), 0);
    assert_eq!(one("SELECT count(*) FROM kb_fts WHERE kb_fts MATCH 'lone'"), 0);
    assert_eq!(json("SELECT textdb_empty_trash()")["items"], 0);

    let purges: Vec<_> = feed(&conn, 0).into_iter().filter(|r| r.1 == "purge").map(|r| (r.2, r.7)).collect();
    assert_eq!(
        purges,
        vec![("/old".to_string(), Some("human".to_string())), ("/lone.md".to_string(), Some("ops".to_string()))]
    );
}

#[test]
fn renames_moves_and_deletes_are_recorded_for_every_node_they_touch() {
    let conn = setup();
    let run = |sql: &str| conn.query_row(sql, [], |_| Ok(())).unwrap();
    let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    let write = |path: &str, content: &str| {
        conn.query_row("SELECT textdb_write(?1, ?2)", [path, content], |_| Ok(())).unwrap();
    };
    type Event = (String, String, Option<String>, Option<String>, Option<i64>, Option<String>);
    let events = |sql: &str| -> Vec<Event> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let history = |path: &str| events(&format!("SELECT op, old_path, new_path, via, version, author FROM textdb_path_history('{path}')"));
    let s = |v: &str| Some(v.to_string());

    write("/a/x.md", "x\n");
    write("/a/x.md", "x2\n");
    write("/a/sub/y.md", "y\n");
    run("SELECT textdb_move('/a/x.md', '/a/renamed.md', 'human')");
    run("SELECT textdb_move('/a', '/b/a2', 'agent-7')");
    run("SELECT textdb_delete('/b/a2/sub', 'ops')");

    // A file's history follows it through its own rename and its folder's move.
    assert_eq!(
        history("/b/a2/renamed.md"),
        vec![
            ("rename".into(), "/a/x.md".into(), s("/a/renamed.md"), None, Some(2), s("human")),
            ("move".into(), "/a/renamed.md".into(), s("/b/a2/renamed.md"), s("/a"), Some(2), s("agent-7")),
        ]
    );
    assert_eq!(history("/b/a2"), vec![("move".into(), "/a".into(), s("/b/a2"), None, None, s("agent-7"))]);
    // A deleted file is found at the path it was deleted from, or by id.
    assert_eq!(
        history("/b/a2/sub/y.md"),
        vec![
            ("move".into(), "/a/sub/y.md".into(), s("/b/a2/sub/y.md"), s("/a"), Some(1), s("agent-7")),
            ("delete".into(), "/b/a2/sub/y.md".into(), None, s("/b/a2/sub"), Some(1), s("ops")),
        ]
    );
    let y = one("SELECT id FROM kb_node WHERE path = '/b/a2/sub/y.md'");
    assert_eq!(one(&format!("SELECT count(*) FROM textdb_path_history(NULL, {y})")), 2);
    // Versions are untouched: a path event is not a version.
    assert_eq!(one("SELECT version FROM kb WHERE path = '/b/a2/renamed.md'"), 2);

    // The store setting turns it off; a handle can decide for itself.
    let text = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, Option<String>>(0));
    assert_eq!(text("SELECT textdb_setting('path_history')").unwrap(), None);
    assert_eq!(text("SELECT textdb_setting('path_history', 'OFF')").unwrap().as_deref(), Some("off"));
    let before = one("SELECT count(*) FROM kb_path_event");
    run("SELECT textdb_move('/b/a2/renamed.md', '/b/a2/quiet.md')");
    assert_eq!(one("SELECT count(*) FROM kb_path_event"), before);
    let db = TextDb::attach(&conn, "kb_", true).with_path_history(Some(true));
    db.rename_by("/b/a2/quiet.md", "/b/a2/loud.md", Some("cli")).unwrap();
    assert_eq!(one("SELECT count(*) FROM kb_path_event"), before + 1);
    assert_eq!(text("SELECT textdb_setting('path_history', NULL)").unwrap(), None);
    for bad in ["SELECT textdb_setting('colour')", "SELECT textdb_setting('path_history', 'maybe')"] {
        let err = text(bad).unwrap_err().to_string();
        assert!(err.starts_with("TX004"), "{err}");
    }

    // Purging removes the events with the node.
    run("SELECT textdb_empty_trash()");
    assert_eq!(one(&format!("SELECT count(*) FROM kb_path_event WHERE node_id = {y}")), 0);
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
fn listings_carry_words_authors_and_folder_totals() {
    let conn = textdb_sqlite::open_in_memory().unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
    let run = |sql: &str| conn.query_row(sql, [], |_| Ok(())).unwrap();
    // Every live folder's totals equal an aggregate over its subtree, computed from scratch.
    let consistent = || {
        let wrong: i64 = conn
            .query_row(
                "SELECT count(*) FROM kb_node f WHERE f.kind = 0 AND f.deleted_at IS NULL AND \
                 (f.t_files, f.t_folders, f.t_bytes, f.t_lines, f.t_words, f.t_versions) <> \
                 (SELECT count(*) FILTER (WHERE kind = 1), count(*) FILTER (WHERE kind = 0), coalesce(sum(nbytes), 0), \
                         coalesce(sum(nlines), 0), coalesce(sum(nwords), 0), coalesce(sum(CASE kind WHEN 1 THEN version END), 0) \
                  FROM kb_node n WHERE n.deleted_at IS NULL AND n.path <> '/' AND (f.path = '/' OR n.path LIKE f.path || '/%'))",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wrong, 0, "folder totals drifted from their subtrees");
    };
    let folder = |dir: &str, name: &str| -> (i64, i64, i64, i64, i64, i64) {
        conn.query_row(
            "SELECT files, folders, nbytes, nlines, nwords, versions FROM textdb_ls(?1) WHERE name = ?2",
            [dir, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap()
    };

    run("SELECT textdb_write('/docs/a.md', '# Title' || char(10) || 'one two three' || char(10), NULL, 'alice')");
    run("SELECT textdb_write('/docs/deep/b.md', 'four five' || char(10), NULL, 'bob')");
    run("SELECT textdb_append('/docs/a.md', 'six' || char(10), 'bob')");
    run("SELECT textdb_append('/docs/a.md', 'seven eight' || char(10), 'bob')");
    run("SELECT textdb_replace_lines('/docs/a.md', 2, 2, 'one' || char(10), NULL, 'carol')");
    // a.md is "# Title\none\nsix\nseven eight\n": 28 bytes, 4 lines, 6 words, 4 versions.
    let (words, versions, nauthors, authors): (i64, i64, i64, String) = conn
        .query_row("SELECT nwords, versions, nauthors, authors FROM textdb_ls('/docs') WHERE name = 'a.md'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap();
    assert_eq!((words, versions, nauthors), (6, 4, 3));
    let authors: serde_json::Value = serde_json::from_str(&authors).unwrap();
    assert_eq!((authors[0]["author"].as_str(), authors[0]["commits"].as_i64()), (Some("bob"), Some(2)));
    assert_eq!(folder("/", "docs"), (2, 1, 38, 5, 8, 5));
    consistent();

    run("SELECT textdb_move('/docs/deep', '/archive/deep', 'dave')");
    assert_eq!(folder("/", "docs"), (1, 0, 28, 4, 6, 4));
    assert_eq!(folder("/", "archive"), (1, 1, 10, 1, 2, 1));
    consistent();

    run("SELECT textdb_delete('/docs/a.md', 'dave')");
    assert_eq!(folder("/", "docs"), (0, 0, 0, 0, 0, 0));
    let root: (i64, i64) = conn.query_row("SELECT t_files, t_folders FROM kb_node WHERE path = '/'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(root, (1, 3));
    consistent();

    let mut stmt = conn.prepare("SELECT path FROM textdb_ls('/', 1)").unwrap();
    let all: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(all, ["/archive", "/archive/deep", "/archive/deep/b.md", "/docs"]);
    // A file lists no subtree counts; a folder has no authors of its own.
    let (files, authors): (Option<i64>, String) = conn
        .query_row("SELECT files, authors FROM textdb_ls('/archive', 1) WHERE name = 'b.md'", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    let authors: serde_json::Value = serde_json::from_str(&authors).unwrap();
    assert_eq!((files, authors[0]["author"].as_str(), authors[0]["commits"].as_i64()), (None, Some("bob"), Some(1)));
    let folder_authors: String =
        conn.query_row("SELECT authors FROM textdb_ls('/') WHERE name = 'archive'", [], |r| r.get(0)).unwrap();
    assert_eq!(folder_authors, "[]");
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
            "DROP TABLE kb_change; ALTER TABLE kb_commit DROP COLUMN kind; ALTER TABLE kb_commit DROP COLUMN base_version; \
             ALTER TABLE kb_commit DROP COLUMN batch;",
        )
        .unwrap();
        conn.execute_batch("DROP TABLE kb_file_author;").unwrap();
        for column in ["nwords", "nauthors", "t_files", "t_folders", "t_bytes", "t_lines", "t_words", "t_versions", "t_updated_at"] {
            conn.execute_batch(&format!("ALTER TABLE kb_node DROP COLUMN {column};")).unwrap();
        }
    }
    let conn = textdb_sqlite::open(path).unwrap();
    let added: i64 = conn.query_row("SELECT textdb_migrate()", [], |r| r.get(0)).unwrap();
    // commit.kind, commit.base_version and commit.batch, and nine node columns for listings
    // (change is created whole, with its batch column).
    assert_eq!(added, 12);
    // ... computed from what the store already holds.
    let (words, authors): (i64, i64) = conn
        .query_row("SELECT nwords, nauthors FROM kb_node WHERE path = '/a.md'", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((words, authors), (1, 1));
    let root_files: i64 = conn.query_row("SELECT t_files FROM kb_node WHERE path = '/'", [], |r| r.get(0)).unwrap();
    assert_eq!(root_files, 1);
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
