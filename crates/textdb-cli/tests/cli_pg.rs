//! The CLI end to end against Postgres. Runs when `TEXTDB_TEST_PG` is a connection URL for a
//! server with the `textdb_pg` extension installed, as a role that may create databases (CI sets
//! it); each test gets a database of its own. Without it the tests pass without doing anything.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

struct Output {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Output {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", self.stdout, self.stderr))
    }
}

/// A database created for one test and dropped after it.
struct Db {
    admin: String,
    name: String,
    url: String,
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Ok(mut c) = postgres::Client::connect(&self.admin, postgres::NoTls) {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name));
        }
    }
}

fn database() -> Option<Db> {
    let admin = std::env::var("TEXTDB_TEST_PG").ok().filter(|u| !u.is_empty())?;
    static N: AtomicUsize = AtomicUsize::new(0);
    // CREATE DATABASE copies template1 and fails while another session is copying it.
    static CREATING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let name = format!("textdb_cli_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
    let guard = CREATING.lock().unwrap_or_else(|e| e.into_inner());
    let mut c = postgres::Client::connect(&admin, postgres::NoTls).expect("connect to TEXTDB_TEST_PG");
    c.batch_execute(&format!("CREATE DATABASE {name}")).expect("create a test database");
    drop(c);
    drop(guard);
    let base = admin.rsplit_once('/').map_or(admin.as_str(), |(base, _)| base);
    let url = format!("{base}/{name}");
    let db = Db { admin: admin.clone(), name, url };
    ok(textdb(&db).arg("init"), None);
    Some(db)
}

fn textdb(db: &Db) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_textdb"));
    cmd.env_remove("TEXTDB_STORE")
        .env_remove("TEXTDB_AUTHOR")
        .env_remove("TEXTDB_PATH_HISTORY")
        .arg("--store")
        .arg(&db.url);
    cmd
}

fn run(cmd: &mut Command, stdin: Option<&str>) -> Output {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn textdb");
    let mut pipe = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        pipe.write_all(text.as_bytes()).unwrap();
    }
    drop(pipe);
    let o = child.wait_with_output().unwrap();
    Output {
        status: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn ok(cmd: &mut Command, stdin: Option<&str>) -> Output {
    let o = run(cmd, stdin);
    assert_eq!(o.status, 0, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    o
}

#[test]
fn write_read_move_and_history() {
    let Some(db) = database() else { return };
    ok(textdb(&db).args(["write", "/notes/a.md", "-m", "first"]), Some("# A\n\none\n"));
    ok(textdb(&db).args(["replace-lines", "/notes/a.md", "3", "3", "--text", "two\n", "-m", "second"]), None);
    assert_eq!(ok(textdb(&db).args(["cat", "/notes/a.md"]), None).stdout, "# A\n\ntwo\n");
    ok(textdb(&db).args(["mv", "/notes/a.md", "/archive/a.md"]), None);
    let listed = ok(textdb(&db).args(["--json", "ls", "/archive"]), None).json();
    assert_eq!(listed[0]["name"], "a.md", "{listed}");
    let history = ok(textdb(&db).args(["--json", "history", "--versions-only", "/archive/a.md"]), None).json();
    let messages: Vec<&str> = history.as_array().unwrap().iter().map(|c| c["message"].as_str().unwrap_or("")).collect();
    assert_eq!(messages, ["first", "second"]);
    assert_eq!(history[1]["nbytes"], 9, "{history}");
    ok(textdb(&db).args(["edit", "/archive/a.md", "--old", "two", "--new", "three", "-m", "third"]), None);
    ok(textdb(&db).args(["append", "/archive/a.md", "four", "-m", "fourth"]), None);
    let feed = ok(textdb(&db).args(["--json", "log", "--limit", "20"]), None).stdout;
    assert!(feed.contains("\"third\"") && feed.contains("\"fourth\""), "{feed}");
    let hit = ok(textdb(&db).args(["--json", "search", "three four"]), None).json();
    assert_eq!(hit[0]["line"], 3, "{hit}");
}

#[test]
fn links_resolve_and_stay_current_as_files_come_and_go() {
    let Some(db) = database() else { return };
    let status = |db: &Db| -> Vec<(String, Option<String>)> {
        let rows = ok(textdb(db).args(["--json", "links", "/acc/acme.md"]), None).json();
        rows.as_array()
            .unwrap()
            .iter()
            .map(|r| (r["status"].as_str().unwrap().to_string(), r["resolved"].as_str().map(str::to_string)))
            .collect()
    };
    let s = |st: &str, to: Option<&str>| (st.to_string(), to.map(str::to_string));
    ok(textdb(&db).args(["write", "/notes/Plan.md"]), Some("# Plan\n\n## Next steps\n"));
    ok(
        textdb(&db).args(["write", "/acc/acme.md"]),
        Some("[[Plan]] [[Plan#Next steps]] [[plan#Nope]]\n[x](../notes/Plan.md) [[Missing]] ![[deck.pdf]]\n[g](https://x.y) | [[notes/Plan\\|p]] |\n"),
    );
    let plan = Some("/notes/Plan.md");
    assert_eq!(
        status(&db),
        [s("ok", plan), s("ok", plan), s("anchor-missing", plan), s("ok", plan), s("broken", None), s("not-in-store", None), s("external", None), s("ok", plan)]
    );
    assert_eq!(ok(textdb(&db).args(["--json", "backlinks", "/notes/Plan.md"]), None).json().as_array().unwrap().len(), 5);
    let broken = ok(textdb(&db).args(["links", "--broken", "/acc"]), None).stdout;
    assert_eq!(
        broken,
        "/acc/acme.md:1: [[plan#Nope]] -> /notes/Plan.md (anchor-missing)\n/acc/acme.md:2: [[Missing]] (broken)\n/acc/acme.md:2: ![[deck.pdf]] (not-in-store)\n"
    );
    let tmp = tempfile::tempdir().unwrap();
    let disk = tmp.path().join("vault");
    std::fs::create_dir_all(disk.join("acc")).unwrap();
    std::fs::write(disk.join("acc/deck.pdf"), b"%PDF").unwrap();
    let checked = ok(textdb(&db).args(["--json", "links", "--broken", "--dir", disk.to_str().unwrap()]), None).json();
    assert_eq!(checked.as_array().unwrap().len(), 2, "{checked}");

    ok(textdb(&db).args(["write", "/Missing.md"]), Some("here\n"));
    assert_eq!(status(&db)[4], s("ok", Some("/Missing.md")));
    ok(textdb(&db).args(["write", "/other/Plan.md"]), Some("# Other\n"));
    assert_eq!(status(&db)[0], s("ambiguous", plan));
    ok(textdb(&db).args(["rm", "/other/Plan.md"]), None);
    assert_eq!(status(&db)[0], s("ok", plan));

    ok(textdb(&db).args(["mv", "/notes/Plan.md", "/archive/Plan-old.md"]), None);
    let after = status(&db);
    assert_eq!((after[0].0.as_str(), after[3].0.as_str(), after[7].0.as_str()), ("broken", "broken", "broken"));
    let view = ok(textdb(&db).args(["--json", "sql", "SELECT kind, alias, status, resolved FROM links WHERE target = 'Missing'"]), None).json();
    assert_eq!(view["rows"][0]["resolved"], "/Missing.md", "{view}");

    ok(textdb(&db).args(["write", "/acc/folders.md"]), Some("[[archive/]] [a](../archive) [[nowhere/]]\n"));
    let folders = ok(textdb(&db).args(["--json", "links", "/acc/folders.md"]), None).json();
    let statuses: Vec<&str> = folders.as_array().unwrap().iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["folder", "folder", "broken"]);
}

#[test]
fn moves_report_or_rewrite_the_links_that_pointed_at_what_moved() {
    let Some(db) = database() else { return };
    let cat = |p: &str| ok(textdb(&db).args(["cat", p]), None).stdout;
    let acme = "See [[Plan]], [[Plan#Next steps|next]] and [plan](../notes/Plan.md#next-steps).\n| [[notes/Plan\\|p]] |\n";
    let index = "[[Plan]] ![[Plan]] [p](Plan.md)\n";
    ok(textdb(&db).args(["write", "/notes/Plan.md"]), Some("# Plan\n\n## Next steps\n"));
    ok(textdb(&db).args(["write", "/acc/acme.md"]), Some(acme));
    ok(textdb(&db).args(["write", "/notes/index.md"]), Some(index));

    let moved = ok(textdb(&db).args(["mv", "/notes/Plan.md", "/archive/2026/Plan-v1.md"]), None).stdout;
    assert!(moved.contains("7 links in 2 files pointed to what moved from /notes/Plan.md and no longer do"), "{moved}");
    assert!(moved.contains("  /acc/acme.md:1: [[Plan]] (now /archive/2026/Plan-v1.md)\n"), "{moved}");
    assert_eq!((cat("/acc/acme.md"), cat("/notes/index.md")), (acme.to_string(), index.to_string()));

    let back = ok(textdb(&db).args(["mv", "--no-update-links", "/archive/2026/Plan-v1.md", "/notes/Plan.md"]), None).stdout;
    assert!(!back.contains("links"), "{back}");
    assert_eq!(ok(textdb(&db).args(["--json", "links", "--broken"]), None).json(), serde_json::json!([]));

    let rewrote = ok(textdb(&db).args(["mv", "--update-links", "/notes/Plan.md", "/archive/2026/Plan-v1.md"]), None).stdout;
    assert!(rewrote.contains("rewrote 7 links in 2 files"), "{rewrote}");
    assert_eq!(
        cat("/acc/acme.md"),
        "See [[Plan-v1]], [[Plan-v1#Next steps|next]] and [plan](../archive/2026/Plan-v1.md#next-steps).\n| [[archive/2026/Plan-v1\\|p]] |\n"
    );
    assert_eq!(cat("/notes/index.md"), "[[Plan-v1]] ![[Plan-v1]] [p](../archive/2026/Plan-v1.md)\n");
    assert_eq!(ok(textdb(&db).args(["--json", "links", "--broken"]), None).json(), serde_json::json!([]));
    let history = ok(textdb(&db).args(["--json", "history", "--versions-only", "/acc/acme.md"]), None).json();
    assert_eq!(history[1]["message"], "links: /notes/Plan.md -> /archive/2026/Plan-v1.md");

    ok(textdb(&db).args(["setting", "link_updates", "rewrite"]), None);
    assert_eq!(ok(textdb(&db).args(["setting", "link_updates"]), None).stdout, "link_updates  rewrite\n");
    assert_eq!(run(textdb(&db).args(["setting", "link_updates", "sometimes"]), None).status, 6);
    ok(textdb(&db).args(["mv", "/archive/2026/Plan-v1.md", "/archive/My Plan.md"]), None);
    assert_eq!(
        cat("/acc/acme.md"),
        "See [[My Plan]], [[My Plan#Next steps|next]] and [plan](../archive/My%20Plan.md#next-steps).\n| [[archive/My Plan\\|p]] |\n"
    );
    let deleted = ok(textdb(&db).args(["rm", "/archive/My Plan.md", "-m", "retired"]), None).stdout;
    assert!(deleted.contains("7 links in 2 files pointed here and are now broken"), "{deleted}");
}

#[test]
fn several_line_ranges_and_meta_make_one_commit_each() {
    let Some(db) = database() else { return };
    ok(textdb(&db).args(["write", "/r.md"]), Some("one\ntwo\nthree\nfour\nfive\n"));
    let ranges = r#"[{"from": 5, "to": 5, "text": "FIVE\n"}, {"from": 1, "to": 1, "text": "ONE\n"}, {"from": 3, "to": 2, "text": "2.5\n"}]"#;
    let w = ok(textdb(&db).args(["--json", "replace-lines", "/r.md", "-b", "1", "--stdin-json", "-m", "tidy"]), Some(ranges)).json();
    assert_eq!((w["version"].as_i64(), w["kind"].as_str()), (Some(2), Some("direct")), "{w}");
    assert_eq!(ok(textdb(&db).args(["cat", "/r.md"]), None).stdout, "ONE\ntwo\n2.5\nthree\nfour\nFIVE\n");
    let history = ok(textdb(&db).args(["--json", "history", "--versions-only", "/r.md"]), None).json();
    assert_eq!(history[1]["message"], "tidy");
    let overlap = run(textdb(&db).args(["replace-lines", "/r.md", "--stdin-json"]), Some(r#"[{"from": 2, "to": 3, "text": ""}, {"from": 3, "to": 4, "text": ""}]"#));
    assert_eq!(overlap.status, 6, "{}", overlap.stderr);
    let outside = run(textdb(&db).args(["replace-lines", "/r.md", "--stdin-json"]), Some(r#"[{"from": 1, "to": 1, "text": "x\n"}, {"from": 20, "to": 20, "text": ""}]"#));
    assert_eq!(outside.status, 6, "{}", outside.stderr);
    ok(textdb(&db).args(["append", "/r.md", "six"]), None);
    let stale = ok(textdb(&db).args(["--json", "replace-lines", "/r.md", "-b", "2", "--stdin-json"]), Some(r#"[{"from": 2, "to": 2, "text": "TWO\n"}, {"from": 4, "to": 4, "text": "THREE\n"}]"#)).json();
    assert_eq!(stale["kind"], "rebased", "{stale}");
    assert_eq!(ok(textdb(&db).args(["cat", "/r.md"]), None).stdout, "ONE\nTWO\n2.5\nTHREE\nfour\nFIVE\nsix\n");

    let doc = "---\r\ntitle: Acme\r\ntags:\r\n    - telco\r\n    - cvm\r\nstatus: draft # was review\r\nowner: \"[[Jonas]]\"\r\n---\r\n# Acme\r\n";
    ok(textdb(&db).args(["write", "/m.md"]), Some(doc));
    assert_eq!(ok(textdb(&db).args(["meta", "get", "/m.md", "tags"]), None).stdout, "telco\ncvm\n");
    ok(textdb(&db).args(["meta", "set", "/m.md", "status", "published"]), None);
    ok(textdb(&db).args(["meta", "set", "/m.md", "tags", "telco", "cvm", "rfi"]), None);
    ok(textdb(&db).args(["meta", "unset", "/m.md", "owner", "-m", "no owner"]), None);
    assert_eq!(
        ok(textdb(&db).args(["cat", "/m.md"]), None).stdout,
        "---\r\ntitle: Acme\r\ntags:\r\n    - telco\r\n    - cvm\r\n    - rfi\r\nstatus: published\r\n---\r\n# Acme\r\n"
    );
    let history = ok(textdb(&db).args(["--json", "history", "--versions-only", "/m.md"]), None).json();
    let messages: Vec<&str> = history.as_array().unwrap().iter().map(|c| c["message"].as_str().unwrap_or("")).collect();
    assert_eq!(messages[1..], ["meta set status", "meta set tags", "no owner"]);
}

#[test]
fn sql_bulk_edits_dry_runs_batches_and_revert() {
    let Some(db) = database() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let a = "---\nentity_name: '[''Account Name'']'\n---\n# Acme teo-group\n\n- item one\n- item two\nAcme Acme\n";
    ok(textdb(&db).args(["write", "/p/a.md"]), Some(a));
    ok(textdb(&db).args(["write", "/p/b.md"]), Some("Acme once\n"));
    ok(textdb(&db).args(["write", "/p/old/c.md"]), Some("gone soon\n"));
    let sql = |args: &[&str]| run(textdb(&db).args(["--author", "claude", "sql"]).args(args), None);

    let fm = sql(&["--format", "tsv", "SELECT data->>'entity_name' AS name FROM frontmatter"]);
    assert_eq!(fm.stdout, "name\n['Account Name']\n", "{}", fm.stderr);
    let files = sql(&["--format", "tsv", "SELECT path, dir, depth, ext FROM files ORDER BY path COLLATE \"C\""]);
    assert_eq!(files.stdout, "path\tdir\tdepth\text\n/p/a.md\t/p\t2\tmd\n/p/b.md\t/p\t2\tmd\n/p/old/c.md\t/p/old\t3\tmd\n");

    // Reads need no --write; a write without it is refused.
    let refused = sql(&["SELECT kb.append('/p/b.md', 'x', :author)"]);
    assert_eq!(refused.status, 6, "{}", refused.stderr);
    assert!(refused.stderr.contains("--write"), "{}", refused.stderr);

    let replace_acme = "SELECT kb.replace(path, 'Acme', 'Globex', NULL, :author) AS v FROM files \
                        WHERE path LIKE '/p/%' AND strpos(kb.content(path), 'Acme') > 0 ORDER BY path COLLATE \"C\"";
    let preview = sql(&["--write", "--dry-run", replace_acme]);
    assert_eq!(preview.status, 0, "{}", preview.stderr);
    assert!(preview.stdout.contains("dry run: 2 changes") && preview.stdout.contains("-Acme once") && preview.stdout.contains("+Globex once"), "{}", preview.stdout);
    assert_eq!(ok(textdb(&db).args(["cat", "/p/b.md"]), None).stdout, "Acme once\n");
    let wrong = sql(&["--write", "SELECT kb.replace('/p/a.md', 'Acme', 'Globex', 2, :author)"]);
    assert_eq!(wrong.status, 6, "{}", wrong.stdout);
    assert!(wrong.stderr.contains("expected 2 occurrences") && wrong.stderr.contains("found 3"), "{}", wrong.stderr);

    let file = tmp.path().join("bulk.sql");
    std::fs::write(
        &file,
        "SELECT kb.replace_many('/p/a.md', '[[\"Acme\", \"Globex\", 3], {\"old\": \"- item\", \"new\": \"- point\"}]', :author) AS a,\n\
         kb.move('/p/b.md', '/p/b2.md', :author)::text AS m,\n\
         kb.remove('/p/old', :author)::text AS d;\n",
    )
    .unwrap();
    let wrote = ok(textdb(&db).args(["--json", "--author", "claude", "sql", "--write", "-f"]).arg(&file), None).json();
    assert_eq!(wrote["store_changes"], 3, "{wrote}");
    let batch = wrote["batch"].as_str().unwrap().to_string();
    assert!(ok(textdb(&db).args(["cat", "/p/a.md"]), None).stdout.contains("# Globex teo-group\n\n- point one\n- point two\nGlobex Globex\n"));
    assert_eq!(sql(&["--format", "lines", "SELECT path FROM commits WHERE batch = $1", "-p", &batch]).stdout, "/p/a.md\n");
    let history = ok(textdb(&db).args(["--json", "history", "/p/a.md"]), None).stdout;
    assert!(history.contains("\"claude\""), "{history}");

    ok(textdb(&db).args(["edit", "/p/a.md", "--old", "- point two", "--new", "- point 2"]), None);
    let refused = run(textdb(&db).args(["revert-batch", &batch]), None);
    assert_eq!(refused.status, 6, "{}", refused.stdout);
    assert!(refused.stderr.contains("/p/a.md") && refused.stderr.contains("--skip-changed"), "{}", refused.stderr);
    ok(textdb(&db).args(["stat", "/p/b2.md"]), None);
    let dry = ok(textdb(&db).args(["--json", "revert-batch", "--skip-changed", "--dry-run", &batch]), None).json();
    assert_eq!(dry["moved_back"], serde_json::json!([{ "from": "/p/b2.md", "to": "/p/b.md" }]), "{dry}");
    assert_eq!(run(textdb(&db).args(["stat", "/p/b.md"]), None).status, 5);
    let reverted = ok(textdb(&db).args(["revert-batch", "--skip-changed", &batch]), None).stdout;
    assert!(reverted.contains("moved back /p/b2.md -> /p/b.md") && reverted.contains("recreated /p/old/c.md") && reverted.contains("skipped: /p/a.md"), "{reverted}");
    assert_eq!(ok(textdb(&db).args(["cat", "/p/b.md"]), None).stdout, "Acme once\n");
    assert_eq!(ok(textdb(&db).args(["cat", "/p/old/c.md"]), None).stdout, "gone soon\n");

    let second = sql(&["--write", "SELECT kb.replace('/p/b.md', 'once', 'twice', 1, :author) AS v"]);
    assert_eq!(second.status, 0, "{}", second.stderr);
    let id = second.stdout.lines().find_map(|l| l.strip_prefix("batch ")).and_then(|l| l.split(':').next()).unwrap().to_string();
    let undone = ok(textdb(&db).args(["--json", "revert-batch", &id]), None).json();
    assert_eq!(undone["restored"][0]["path"], "/p/b.md", "{undone}");
    assert_eq!(ok(textdb(&db).args(["cat", "/p/b.md"]), None).stdout, "Acme once\n");
    assert_eq!(run(textdb(&db).args(["revert-batch", "20000101-000000-dead"]), None).status, 5);

    // A writing statement is all or nothing.
    ok(textdb(&db).args(["write", "/notes/a-draft.md"]), Some("status: draft\n"));
    ok(textdb(&db).args(["write", "/notes/b-other.md"]), Some("no status\n"));
    let partial = sql(&["--write", "SELECT kb.edit(path, 'status: draft', 'status: done', :author) FROM files WHERE path LIKE '/notes/%' ORDER BY path COLLATE \"C\""]);
    assert_eq!(partial.status, 6, "{}", partial.stdout);
    assert_eq!(ok(textdb(&db).args(["cat", "/notes/a-draft.md"]), None).stdout, "status: draft\n");
}

#[test]
fn assets_push_pull_verify_and_links() {
    let Some(db) = database() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("vault");
    let bucket = tmp.path().join("bucket");
    let config = tmp.path().join("config");
    for d in ["notes", "img", "docs"] {
        std::fs::create_dir_all(vault.join(d)).unwrap();
    }
    std::fs::create_dir_all(&bucket).unwrap();
    std::fs::write(vault.join("notes/a.md"), "![[arch.png]]\n[deck](../docs/deck.pdf)\n").unwrap();
    std::fs::write(vault.join("img/arch.png"), [137u8, 80, 78, 71, 0, 1]).unwrap();
    std::fs::write(vault.join("docs/deck.pdf"), b"%PDF-1.4 one").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&db);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let status = || ok(&mut t(&["--json", "assets", "status"]), None).json();
    let first = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert_eq!(first.status, 0, "{}", first.stderr);
    assert_eq!(status()["counts"], serde_json::json!({ "new": 2 }));
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);

    let pushed = ok(&mut t(&["--json", "assets", "push", "-m", "first assets"]), None).json();
    assert_eq!(pushed["pushed"].as_array().unwrap().len(), 2, "{pushed}");
    let pointer = std::fs::read_to_string(vault.join("img/arch.png.tdbasset")).unwrap();
    assert_eq!(ok(&mut t(&["cat", "/img/arch.png.tdbasset"]), None).stdout, pointer);
    let links = ok(&mut t(&["--json", "links", "/notes/a.md"]), None).json();
    assert_eq!(
        (links[0]["status"].as_str(), links[0]["resolved"].as_str(), links[0]["asset"].as_bool()),
        (Some("ok"), Some("/img/arch.png"), Some(true)),
        "{links}"
    );
    let view = ok(&mut t(&["--json", "sql", "SELECT resolved, asset FROM links WHERE kind = 'md'"]), None).json();
    assert_eq!(view["rows"][0], serde_json::json!({ "resolved": "/docs/deck.pdf", "asset": true }), "{view}");
    assert_eq!(ok(&mut t(&["--json", "backlinks", "/docs/deck.pdf"]), None).json().as_array().unwrap().len(), 1);
    let again = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert_eq!(again.status, 0, "{}", again.stdout);

    std::fs::write(vault.join("img/arch.png"), [137u8, 80, 78, 71, 0, 2, 3]).unwrap();
    ok(&mut t(&["assets", "push", "/img"]), None);
    let pointer2 = std::fs::read_to_string(vault.join("img/arch.png.tdbasset")).unwrap();
    let id = |p: &str| p.lines().find_map(|l| l.strip_prefix("id: ")).unwrap().to_string();
    assert_eq!(id(&pointer2), id(&pointer));

    std::fs::remove_file(vault.join("docs/deck.pdf")).unwrap();
    assert_eq!(status()["counts"], serde_json::json!({ "not-pulled": 1, "ok": 1 }));
    ok(&mut t(&["assets", "pull"]), None);
    assert_eq!(std::fs::read(vault.join("docs/deck.pdf")).unwrap(), b"%PDF-1.4 one");
    assert_eq!(ok(&mut t(&["--json", "assets", "verify"]), None).json()["problems"], 0);
    ok(&mut t(&["mv", "--update-links", "/docs/deck.pdf.tdbasset", "/archive/deck.pdf.tdbasset"]), None);
    assert_eq!(ok(&mut t(&["cat", "/notes/a.md"]), None).stdout, "![[arch.png]]\n[deck](../archive/deck.pdf)\n");
    assert_eq!(ok(&mut t(&["--json", "assets", "stores"]), None).json()[0]["name"], "team");
}

#[test]
fn sync_pairs_assets_and_pushes_and_pulls_them() {
    let Some(db) = database() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (v1, v2, bucket, config) = (tmp.path().join("v1"), tmp.path().join("v2"), tmp.path().join("bucket"), tmp.path().join("config"));
    for d in [v1.join("img"), v2.clone(), bucket.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let t = |args: &[&str]| {
        let mut c = textdb(&db);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let (d1, d2) = (v1.to_str().unwrap(), v2.to_str().unwrap());
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    std::fs::write(v1.join("notes.md"), "![[a.png]]\n").unwrap();
    std::fs::write(v1.join("img/a.png"), b"\x89PNG A").unwrap();
    std::fs::write(v1.join("img/b.png"), b"\x89PNG B").unwrap();

    let s1 = ok(&mut t(&["--json", "sync", "--push", "/", d1]), None).json();
    assert_eq!(s1["assets"]["pushed"], serde_json::json!(["/img/a.png", "/img/b.png"]), "{s1}");

    // The settings decide when the command line does not: pull, and all of them.
    ok(&mut t(&["setting", "asset_sync", "pull"]), None);
    ok(&mut t(&["setting", "asset_pull", "all"]), None);
    let s2 = ok(&mut t(&["--json", "sync", "/", d2]), None).json();
    assert_eq!((s2["assets"]["mode"].as_str(), &s2["assets"]["pulled"]), (Some("pull"), &serde_json::json!(["/img/a.png", "/img/b.png"])), "{s2}");

    // Moved and deleted in the store: the files follow on disk.
    ok(&mut t(&["mv", "/img/a.png", "/pics/a.png"]), None);
    ok(&mut t(&["rm", "/img/b.png"]), None);
    let s3 = ok(&mut t(&["--json", "sync", "/", d2]), None).json();
    assert!(v2.join("pics/a.png").exists() && !v2.join("img/b.png").exists(), "{s3}");
    assert_eq!(s3["assets"]["trashed"], serde_json::json!(["img/b.png"]), "{s3}");

    // A push records its pointers in the sync base, so the next sync has nothing to take in.
    ok(&mut t(&["sync", "/", d1]), None);
    assert!(v1.join("pics/a.png").exists() && !v1.join("img").exists(), "img/ is left empty and removed");
    std::fs::create_dir_all(v1.join("img")).unwrap();
    std::fs::write(v1.join("img/c.png"), b"\x89PNG C").unwrap();
    ok(&mut t(&["assets", "push", "--dir", d1]), None);
    let quiet = ok(&mut t(&["--json", "sync", "--dry-run", "/", d1]), None).json();
    assert_eq!((&quiet["to_textdb"]["new"], &quiet["to_disk"]["new"]), (&serde_json::json!([]), &serde_json::json!([])), "{quiet}");
}

#[test]
fn sync_reconciles_both_sides_merges_and_marks_conflicts() {
    let Some(db) = database() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("notes");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let doc = "one\ntwo\nthree\nfour\nfive\n";
    for name in ["a.md", "b.md", "c.md", "sub/d.md"] {
        std::fs::write(dir.join(name), doc).unwrap();
    }
    std::fs::write(dir.join("logo.png"), [0u8, 1, 2]).unwrap();
    let sync = |extra: &[&str]| {
        let mut cmd = textdb(&db);
        cmd.args(["--json", "sync"]).args(extra).arg("/notes").arg(&dir);
        run(&mut cmd, None)
    };
    let cat = |path: &str| ok(textdb(&db).args(["cat", path]), None).stdout;
    let disk = |rel: &str| std::fs::read_to_string(dir.join(rel)).unwrap();

    let first = sync(&[]);
    assert_eq!(first.status, 0, "{}", first.stderr);
    assert_eq!(first.json()["to_textdb"]["new"], serde_json::json!(["a.md", "b.md", "c.md", "sub/d.md"]));

    ok(textdb(&db).args(["replace-lines", "/notes/a.md", "1", "1", "--text", "ONE\n"]), None);
    std::fs::write(dir.join("a.md"), "one\ntwo\nthree\nfour\nFIVE\n").unwrap();
    ok(textdb(&db).args(["replace-lines", "/notes/b.md", "3", "3", "--text", "three (textdb)\n"]), None);
    std::fs::write(dir.join("b.md"), "one\ntwo\nthree (disk)\nfour\nfive\n").unwrap();
    ok(textdb(&db).args(["rm", "/notes/c.md"]), None);
    std::fs::remove_file(dir.join("sub/d.md")).unwrap();
    std::fs::write(dir.join("moved.md"), doc).unwrap();
    ok(textdb(&db).args(["write", "/notes/e.md"]), Some("new in textdb\n"));
    std::fs::write(dir.join("f.md"), "new on disk\n").unwrap();

    let synced = sync(&[]);
    assert_eq!(synced.status, 3, "conflicts exit with 3: {}", synced.stdout);
    let merged = "ONE\ntwo\nthree\nfour\nFIVE\n";
    assert_eq!((cat("/notes/a.md"), disk("a.md")), (merged.to_string(), merged.to_string()));
    assert_eq!(disk("b.md"), "one\ntwo\n<<<<<<< textdb\nthree (textdb)\n=======\nthree (disk)\n>>>>>>> disk\nfour\nfive\n");
    assert!(!dir.join("c.md").exists());
    assert_eq!(disk("e.md"), "new in textdb\n");
    assert_eq!(cat("/notes/f.md"), "new on disk\n");
    assert_eq!(run(textdb(&db).args(["stat", "/notes/sub/d.md"]), None).status, 5);

    std::fs::write(dir.join("b.md"), "one\ntwo\nthree (both)\nfour\nfive\n").unwrap();
    let resolved = sync(&[]);
    assert_eq!(resolved.status, 0, "{}", resolved.stdout);
    assert_eq!(cat("/notes/b.md"), "one\ntwo\nthree (both)\nfour\nfive\n");
    let quiet = sync(&[]).json();
    assert_eq!(quiet["unchanged"], 5, "{quiet}");
    assert_eq!(std::fs::read(dir.join("logo.png")).unwrap(), [0u8, 1, 2]);
}
