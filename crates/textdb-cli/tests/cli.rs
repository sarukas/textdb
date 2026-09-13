//! The CLI end to end against a SQLite store: import, browse, edit by line number against a
//! version read earlier, conflicts, history, hunks, the change log, and following changes
//! made by another process.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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

fn textdb(store: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_textdb"));
    cmd.env_remove("TEXTDB_STORE")
        .env_remove("TEXTDB_AUTHOR")
        .env_remove("TEXTDB_PATH_HISTORY")
        .arg("--store")
        .arg(store);
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

fn corpus(dir: &Path) {
    let guide: String = (1..=400).map(|i| format!("line {i} of the guide, padded to a realistic width\n")).collect();
    std::fs::create_dir_all(dir.join("guide/deep")).unwrap();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("guide/intro.md"), format!("# Intro\n\n{guide}")).unwrap();
    std::fs::write(dir.join("guide/deep/notes.md"), "# Notes\n\n## Todo\n\n- one\n- two\n").unwrap();
    std::fs::write(dir.join("readme.txt"), "hello\n").unwrap();
    std::fs::write(dir.join("logo.png"), [0u8, 1, 2, 3]).unwrap();
    std::fs::write(dir.join(".git/ignored.md"), "no\n").unwrap();
}

#[test]
fn import_browse_edit_by_line_and_handle_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    corpus(&src);
    let store = tmp.path().join("kb.db");

    let first = ok(textdb(&store).args(["--json", "import"]).arg(&src).args(["--prefix", "/docs"]), None).json();
    assert_eq!(first["stats"]["created"], 3, "{first}");
    let again = ok(textdb(&store).args(["--json", "import"]).arg(&src).args(["--prefix", "/docs"]), None).json();
    assert_eq!((again["stats"]["unchanged"].as_i64(), again["stats"]["created"].as_i64()), (Some(3), Some(0)));

    let tree = ok(textdb(&store).args(["tree", "/docs"]), None).stdout;
    assert!(tree.contains("guide/") && tree.contains("notes.md") && tree.contains("readme.txt"), "{tree}");
    // A shallow tree still counts everything below each folder it shows.
    let top = ok(textdb(&store).args(["tree", "/docs", "-L", "1"]), None).stdout;
    assert!(top.contains("guide/  (2 files,") && !top.contains("intro.md"), "{top}");
    let shallow = ok(textdb(&store).args(["--json", "tree", "/docs", "-L", "1"]), None).json();
    let paths: Vec<&str> = shallow.as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["/docs/guide", "/docs/readme.txt"]);

    // `cat -n` names the version the line numbers belong to.
    let numbered = ok(textdb(&store).args(["cat", "-n", "/docs/guide/intro.md", "--lines", "3:4"]), None).stdout;
    assert!(numbered.starts_with("/docs/guide/intro.md v1 · lines 3-4 of 402\n"), "{numbered}");
    assert!(numbered.contains("     3\tline 1 of the guide"), "{numbered}");
    let section = ok(textdb(&store).args(["cat", "/docs/guide/deep/notes.md", "--section", "Todo"]), None).stdout;
    assert!(section.starts_with("## Todo\n") && section.contains("- two"), "{section}");

    // Someone edits near the end while an agent works from version 1 near the top.
    let human = ok(
        textdb(&store).args(["--json", "--author", "human", "edit", "/docs/guide/intro.md", "--old", "line 390 of", "--new", "LINE 390 of"]),
        None,
    )
    .json();
    assert_eq!((human["version"].as_i64(), human["kind"].as_str()), (Some(2), Some("direct")));
    let agent = ok(
        textdb(&store).args(["--json", "--author", "agent-7", "replace-lines", "/docs/guide/intro.md", "3", "3", "--base-version", "1"]),
        Some("line 1, rewritten by the agent\n"),
    )
    .json();
    assert_eq!((agent["version"].as_i64(), agent["kind"].as_str()), (Some(3), Some("rebased")));

    // A second agent also started from version 1 and rewrote the same line: a conflict,
    // reported with exit status 3 and the current text of that line.
    let clash = run(
        textdb(&store).args(["--json", "--author", "agent-8", "replace-lines", "/docs/guide/intro.md", "3", "3", "-b", "1", "--text", "mine\n"]),
        None,
    );
    assert_eq!(clash.status, 3, "{}\n{}", clash.stdout, clash.stderr);
    let err = clash.json();
    assert_eq!(err["error"]["code"], "TX001");
    assert_eq!(err["error"]["conflict"]["current_version"], 3);
    assert_eq!(err["error"]["conflict"]["theirs"], "line 1, rewritten by the agent\n");

    // Not found and invalid edits have their own statuses.
    assert_eq!(run(textdb(&store).args(["cat", "/nope.md"]), None).status, 5);
    assert_eq!(run(textdb(&store).args(["edit", "/docs/readme.txt", "--old", "absent", "--new", "x"]), None).status, 6);
    // An empty pipe is not taken as "make the file empty".
    assert_eq!(run(textdb(&store).args(["write", "/docs/readme.txt"]), Some("")).status, 6);

    let history = ok(textdb(&store).args(["--json", "history", "/docs/guide/intro.md"]), None).json();
    let kinds: Vec<(i64, &str, Option<i64>)> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["version"].as_i64().unwrap(), c["kind"].as_str().unwrap(), c["base_version"].as_i64()))
        .collect();
    assert_eq!(kinds, [(1, "direct", None), (2, "direct", Some(1)), (3, "rebased", Some(1))]);

    let hunks = ok(textdb(&store).args(["--json", "hunks", "/docs/guide/intro.md"]), None).json();
    assert_eq!((hunks["from"].as_i64(), hunks["to"].as_i64()), (Some(2), Some(3)));
    assert_eq!(hunks["hunks"][0]["new_from"], 3);
    assert_eq!(hunks["hunks"][0]["new_text"], "line 1, rewritten by the agent\n");

    let log = ok(textdb(&store).args(["--json", "log"]), None).json();
    let last = log.as_array().unwrap().last().unwrap().clone();
    assert_eq!((last["op"].as_str(), last["author"].as_str(), last["commit_kind"].as_str()), (Some("commit"), Some("agent-7"), Some("rebased")));

    // Markdown list items start with a hyphen; they are text, not flags.
    ok(textdb(&store).args(["edit", "/docs/guide/deep/notes.md", "--old", "- two", "--new", "- two\n- three"]), None);
    ok(textdb(&store).args(["append", "/docs/guide/deep/notes.md", "- four"]), None);
    ok(textdb(&store).args(["replace-lines", "/docs/guide/deep/notes.md", "5", "5", "--text", "- ONE\n"]), None);
    let notes = ok(textdb(&store).args(["cat", "/docs/guide/deep/notes.md"]), None).stdout;
    assert_eq!(notes, "# Notes\n\n## Todo\n\n- ONE\n- two\n- three\n- four\n");

    // Write from stdin, move, delete.
    ok(textdb(&store).args(["write", "/docs/new.md", "-m", "draft"]), Some("# New\n"));
    ok(textdb(&store).args(["mv", "/docs/new.md", "/archive/new.md"]), None);
    ok(textdb(&store).args(["rm", "/archive"]), None);
    assert_eq!(run(textdb(&store).args(["stat", "/archive/new.md"]), None).status, 5);

    let exported = tmp.path().join("out");
    ok(textdb(&store).args(["export", "/docs"]).arg(&exported), None);
    let intro = std::fs::read_to_string(exported.join("guide/intro.md")).unwrap();
    assert!(intro.contains("line 1, rewritten by the agent\n") && intro.contains("LINE 390 of"));
}

#[test]
fn export_writes_only_what_differs_and_stops_on_clashing_names() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    corpus(&src);
    let store = tmp.path().join("kb.db");
    ok(textdb(&store).arg("import").arg(&src).args(["--prefix", "/docs"]), None);
    ok(textdb(&store).args(["write", "/docs/crlf.md"]), Some("\u{feff}one\r\ntwo\r\n"));

    // A first export writes everything, a second nothing: every file is already identical.
    let out = tmp.path().join("checkout");
    let first = ok(textdb(&store).args(["--json", "export", "/docs"]).arg(&out), None).json();
    assert_eq!((first["new"].as_array().unwrap().len(), first["written"].as_i64()), (4, Some(4)), "{first}");
    assert_eq!(std::fs::read(out.join("crlf.md")).unwrap(), "\u{feff}one\r\ntwo\r\n".as_bytes());
    let again = ok(textdb(&store).args(["--json", "export", "/docs"]).arg(&out), None).json();
    assert_eq!((again["unchanged"].as_i64(), again["written"].as_i64()), (Some(4), Some(0)), "{again}");

    // Change one file in the store and one on disk; add a file only on disk.
    ok(textdb(&store).args(["write", "/docs/readme.txt"]), Some("hello again\n"));
    std::fs::write(out.join("guide/deep/notes.md"), "edited on disk\n").unwrap();
    std::fs::write(out.join("untracked.md"), "only on disk\n").unwrap();
    let dry = ok(textdb(&store).args(["--json", "export", "--dry-run", "/docs"]).arg(&out), None).json();
    assert_eq!(dry["changed"], serde_json::json!(["guide/deep/notes.md", "readme.txt"]), "{dry}");
    assert_eq!((dry["written"].as_i64(), dry["unchanged"].as_i64()), (Some(0), Some(2)));
    assert_eq!(std::fs::read_to_string(out.join("readme.txt")).unwrap(), "hello\n");

    let text = ok(textdb(&store).args(["export", "/docs"]).arg(&out), None).stdout;
    assert!(text.contains("0 new, 2 changed, 2 unchanged; wrote 2 files"), "{text}");
    assert_eq!(std::fs::read_to_string(out.join("readme.txt")).unwrap(), "hello again\n");
    assert_eq!(std::fs::read_to_string(out.join("untracked.md")).unwrap(), "only on disk\n");

    // Names that differ only in case are one file on Windows and macOS: nothing is written there.
    ok(textdb(&store).args(["write", "/docs/README.TXT"]), Some("shouting\n"));
    let clash = run(textdb(&store).args(["--json", "export", "/docs"]).arg(&out), None);
    let report = clash.json();
    let kinds: Vec<&str> = report["problems"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["path"] == "README.TXT")
        .map(|p| p["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"case"), "{report}");
    if cfg!(any(windows, target_os = "macos")) {
        assert_eq!((clash.status, report["stopped"].as_bool(), report["written"].as_i64()), (6, Some(true), Some(0)));
        assert_eq!(std::fs::read_to_string(out.join("readme.txt")).unwrap(), "hello again\n");
    } else {
        assert_eq!((clash.status, report["written"].as_i64()), (0, Some(1)));
    }
}

fn has_git() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().expect("run git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[test]
fn sync_reconciles_both_sides_merges_and_marks_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let dir = tmp.path().join("notes");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let doc = "one\ntwo\nthree\nfour\nfive\n";
    for name in ["a.md", "b.md", "c.md", "sub/d.md"] {
        std::fs::write(dir.join(name), doc).unwrap();
    }
    std::fs::write(dir.join("logo.png"), [0u8, 1, 2]).unwrap();
    let sync = |extra: &[&str]| {
        let mut cmd = textdb(&store);
        cmd.args(["--json", "sync"]).args(extra).arg("/notes").arg(&dir);
        run(&mut cmd, None)
    };
    let cat = |path: &str| ok(textdb(&store).args(["cat", path]), None).stdout;
    let disk = |rel: &str| std::fs::read_to_string(dir.join(rel)).unwrap();

    // The first sync takes in the directory's text files.
    let first = sync(&[]);
    assert_eq!(first.status, 0, "{}", first.stderr);
    let first = first.json();
    assert_eq!(first["to_textdb"]["new"], serde_json::json!(["a.md", "b.md", "c.md", "sub/d.md"]), "{first}");
    assert_eq!(first["first_sync"], true);

    // Edits on both sides: different lines of a.md, the same line of b.md.
    ok(textdb(&store).args(["replace-lines", "/notes/a.md", "1", "1", "--text", "ONE\n"]), None);
    std::fs::write(dir.join("a.md"), "one\ntwo\nthree\nfour\nFIVE\n").unwrap();
    ok(textdb(&store).args(["replace-lines", "/notes/b.md", "3", "3", "--text", "three (textdb)\n"]), None);
    std::fs::write(dir.join("b.md"), "one\ntwo\nthree (disk)\nfour\nfive\n").unwrap();
    // Deleted in textdb, moved on disk, new on each side.
    ok(textdb(&store).args(["rm", "/notes/c.md"]), None);
    std::fs::remove_file(dir.join("sub/d.md")).unwrap();
    std::fs::write(dir.join("moved.md"), doc).unwrap();
    ok(textdb(&store).args(["write", "/notes/e.md"]), Some("new in textdb\n"));
    std::fs::write(dir.join("f.md"), "new on disk\n").unwrap();

    let dry = sync(&["--dry-run"]);
    assert_eq!(dry.status, 0, "{}", dry.stdout);
    let dry = dry.json();
    assert_eq!(dry["merged"], serde_json::json!(["a.md"]), "{dry}");
    assert_eq!(dry["conflicts"], serde_json::json!(["b.md"]));
    assert_eq!(dry["to_disk"]["new"], serde_json::json!(["e.md"]));
    assert_eq!(dry["to_disk"]["deleted"], serde_json::json!(["c.md"]));
    assert_eq!(dry["to_textdb"]["new"], serde_json::json!(["f.md"]));
    assert_eq!(dry["moved"], serde_json::json!([{ "from": "sub/d.md", "to": "moved.md" }]));
    assert_eq!(disk("b.md"), "one\ntwo\nthree (disk)\nfour\nfive\n");

    let synced = sync(&[]);
    assert_eq!(synced.status, 3, "conflicts exit with 3: {}", synced.stdout);
    let merged = "ONE\ntwo\nthree\nfour\nFIVE\n";
    assert_eq!((cat("/notes/a.md"), disk("a.md")), (merged.to_string(), merged.to_string()));
    assert_eq!(disk("b.md"), "one\ntwo\n<<<<<<< textdb\nthree (textdb)\n=======\nthree (disk)\n>>>>>>> disk\nfour\nfive\n");
    assert_eq!(cat("/notes/b.md"), "one\ntwo\nthree (textdb)\nfour\nfive\n");
    assert!(!dir.join("c.md").exists());
    assert_eq!(disk("e.md"), "new in textdb\n");
    assert_eq!(cat("/notes/f.md"), "new on disk\n");
    assert_eq!(run(textdb(&store).args(["stat", "/notes/sub/d.md"]), None).status, 5);
    let moved = ok(textdb(&store).args(["--json", "history", "/notes/moved.md"]), None).stdout;
    assert!(moved.contains("/notes/sub/d.md"), "the move is in the history: {moved}");
    assert!(!dir.join("logo.png").metadata().unwrap().permissions().readonly());

    // Markers left on disk are never taken in.
    let again = sync(&[]);
    assert_eq!(again.status, 3);
    assert_eq!(again.json()["unresolved"], serde_json::json!(["b.md"]));
    assert_eq!(cat("/notes/b.md"), "one\ntwo\nthree (textdb)\nfour\nfive\n");

    // The resolution goes to textdb, and then there is nothing left to do.
    std::fs::write(dir.join("b.md"), "one\ntwo\nthree (both)\nfour\nfive\n").unwrap();
    let resolved = sync(&[]);
    assert_eq!(resolved.status, 0, "{}", resolved.stdout);
    assert_eq!(resolved.json()["to_textdb"]["changed"], serde_json::json!(["b.md"]));
    assert_eq!(cat("/notes/b.md"), "one\ntwo\nthree (both)\nfour\nfive\n");
    let quiet = sync(&[]).json();
    assert_eq!(quiet["unchanged"], 5, "{quiet}");
    assert_eq!(std::fs::read(dir.join("logo.png")).unwrap(), [0u8, 1, 2]);
}

#[test]
fn sync_in_a_git_checkout_credits_git_authors_and_commits_with_trailers() {
    if !has_git() {
        eprintln!("git is not installed; skipped");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let repo = tmp.path().join("repo");
    let docs = repo.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    git(&repo, &["init", "-q"]);
    for (k, v) in [("user.name", "alice"), ("user.email", "alice@example.com"), ("core.autocrlf", "false"), ("commit.gpgsign", "false")] {
        git(&repo, &["config", k, v]);
    }
    for (name, text) in [("a.md", "a1\n"), ("b.md", "b1\n"), ("c.md", "c1\n")] {
        std::fs::write(docs.join(name), text).unwrap();
    }
    std::fs::write(repo.join("README.md"), "outside the synced folder\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let initial = git(&repo, &["rev-parse", "HEAD"]);

    let first = ok(textdb(&store).args(["--json", "sync", "/docs"]).arg(&docs), None).json();
    assert_eq!((first["git"]["commit"].as_str(), first["git"]["clean"].as_bool()), (Some(initial.as_str()), Some(true)), "{first}");

    // Upstream, bob edits a.md and deletes c.md; meanwhile an agent edits b.md in textdb.
    std::fs::write(docs.join("a.md"), "a2\n").unwrap();
    std::fs::remove_file(docs.join("c.md")).unwrap();
    git(&repo, &["-c", "user.name=bob", "-c", "user.email=bob@example.com", "commit", "-q", "-am", "Update a, drop c"]);
    ok(textdb(&store).args(["--author", "agent-7", "write", "/docs/b.md"]), Some("b2\n"));

    let synced = ok(textdb(&store).args(["--json", "--author", "syncer", "sync", "--commit", "/docs"]).arg(&docs), None).json();
    assert_eq!(synced["to_textdb"]["changed"], serde_json::json!(["a.md"]), "{synced}");
    assert_eq!(synced["to_textdb"]["deleted"], serde_json::json!(["c.md"]));
    assert_eq!(synced["to_disk"]["changed"], serde_json::json!(["b.md"]));
    let history = ok(textdb(&store).args(["--json", "history", "/docs/a.md"]), None).stdout;
    assert!(history.contains("\"bob\"") && history.contains("Update a, drop c"), "{history}");
    assert_eq!(run(textdb(&store).args(["stat", "/docs/c.md"]), None).status, 5);

    // The commit holds only what sync wrote, with trailers naming the store state and its authors.
    let head = git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(synced["git"]["committed"].as_str(), Some(head.as_str()));
    assert_eq!(git(&repo, &["show", "--name-only", "--format=", "HEAD"]), "docs/b.md");
    let body = git(&repo, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Textdb-Prefix: /docs") && body.contains("Textdb-Author: agent-7") && body.contains("Textdb-Seq: "), "{body}");

    let status = ok(textdb(&store).args(["--json", "git-status", "/docs"]).arg(&docs), None).json();
    assert_eq!(status["git"]["same"], 2, "{status}");
    assert_eq!((status["git"]["differ"].as_array().unwrap().len(), status["git"]["only_git"].as_array().unwrap().len()), (0, 0));
    assert_eq!(status["synced"][0]["git"]["commit"].as_str(), Some(head.as_str()));
    assert_eq!(status["since_sync"]["changed"].as_array().unwrap().len(), 0);

    // A store imported before the checkout moved on: --base names the commit it came from, so
    // the first sync merges instead of conflicting.
    let older = tmp.path().join("older.db");
    ok(textdb(&older).arg("import").arg(&docs).args(["--prefix", "/docs"]), None);
    std::fs::write(docs.join("a.md"), "a3\n").unwrap();
    git(&repo, &["-c", "user.name=carol", "-c", "user.email=carol@example.com", "commit", "-q", "-am", "a3"]);
    ok(textdb(&older).args(["write", "/docs/b.md"]), Some("b3\n"));
    let boot = ok(textdb(&older).args(["--json", "sync", "--base", "HEAD~1", "/docs"]).arg(&docs), None).json();
    assert_eq!(boot["to_textdb"]["changed"], serde_json::json!(["a.md"]), "{boot}");
    assert_eq!(boot["to_disk"]["changed"], serde_json::json!(["b.md"]));
    assert_eq!(boot["conflicts"], serde_json::json!([]));
    assert_eq!(std::fs::read_to_string(docs.join("b.md")).unwrap(), "b3\n");
    assert_eq!(run(textdb(&older).args(["sync", "--base", "HEAD", "/docs"]).arg(&docs), None).status, 6);
}

#[test]
fn renames_moves_and_deletes_show_in_history_unless_turned_off() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let path_ops = |path: &str| -> Vec<(String, Option<i64>, Option<String>, Option<String>)> {
        ok(textdb(&store).args(["--json", "history", path]), None)
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["type"].as_str().unwrap().to_string(),
                    i["version"].as_i64(),
                    i["op"].as_str().map(String::from),
                    i["author"].as_str().map(String::from),
                )
            })
            .collect()
    };
    let s = |v: &str| Some(v.to_string());

    ok(textdb(&store).args(["write", "/a/x.md"]), Some("one\n"));
    ok(textdb(&store).args(["--author", "human", "mv", "/a/x.md", "/a/y.md"]), None);
    ok(textdb(&store).args(["--author", "agent-7", "mv", "/a", "/b"]), None);
    assert_eq!(
        path_ops("/b/y.md"),
        vec![
            ("version".into(), Some(1), None, s("cli")),
            ("path".into(), Some(1), s("rename"), s("human")),
            // /a and /b are both in the root folder: the folder was renamed, and y.md went with it.
            ("path".into(), Some(1), s("rename"), s("agent-7")),
        ]
    );
    let text = ok(textdb(&store).args(["history", "b/y.md"]), None).stdout;
    assert!(text.contains("renamed") && text.contains("/a/x.md -> /a/y.md"), "{text}");
    assert!(text.contains("/a/y.md -> /b/y.md  (with /a)"), "{text}");
    let versions = ok(textdb(&store).args(["--json", "history", "/b/y.md", "--versions-only"]), None).json();
    assert_eq!(versions.as_array().unwrap().len(), 1);
    assert!(versions[0].get("type").is_none(), "{versions}");

    // Off in the store: nothing recorded, unless one command asks for it.
    ok(textdb(&store).args(["setting", "path_history", "off"]), None);
    let shown = ok(textdb(&store).args(["--json", "setting"]), None).json();
    assert_eq!(shown["path_history"], serde_json::json!({ "value": "off", "effective": false }));
    ok(textdb(&store).args(["mv", "/b/y.md", "/b/z.md"]), None);
    assert_eq!(path_ops("/b/z.md").len(), 3);
    ok(textdb(&store).args(["--path-history", "on", "mv", "/b/z.md", "/b/w.md"]), None);
    assert_eq!(path_ops("/b/w.md").len(), 4);
    ok(textdb(&store).args(["setting", "path_history", "default"]), None);
    let shown = ok(textdb(&store).args(["--json", "setting", "path_history"]), None).json();
    assert_eq!(shown["path_history"], serde_json::json!({ "value": null, "effective": true }));

    // A delete is the last entry, found at the path it was deleted from.
    ok(textdb(&store).args(["--author", "ops", "rm", "/b"]), None);
    let last = ok(textdb(&store).args(["--json", "history", "/b/w.md"]), None).json();
    let last = last.as_array().unwrap().last().unwrap().clone();
    assert_eq!((last["op"].as_str(), last["via"].as_str(), last["author"].as_str()), (Some("delete"), Some("/b"), Some("ops")));

    assert_eq!(run(textdb(&store).args(["setting", "colour"]), None).status, 6);
    assert_eq!(run(textdb(&store).args(["--path-history", "maybe", "ls"]), None).status, 2);
}

#[test]
fn watch_follows_commits_made_by_another_process() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let since = ok(textdb(&store).args(["--json", "init"]), None).json()["last_seq"].as_i64().unwrap();

    let mut watcher = textdb(&store)
        .args(["--json", "watch", "--prefix", "/live", "--since"])
        .arg(since.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = watcher.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines() {
            if tx.send(l.unwrap()).is_err() {
                break;
            }
        }
    });

    ok(textdb(&store).args(["write", "/elsewhere.md"]), Some("not watched\n"));
    ok(textdb(&store).args(["--author", "agent-9", "write", "/live/a.md"]), Some("one\n"));
    ok(textdb(&store).args(["--author", "agent-9", "append", "/live/a.md", "two"]), None);

    let mut seen = Vec::new();
    while seen.len() < 3 {
        let l = rx.recv_timeout(Duration::from_secs(20)).expect("watch printed nothing in time");
        seen.push(serde_json::from_str::<Value>(&l).unwrap());
    }
    watcher.kill().unwrap();
    let ops: Vec<(&str, &str)> = seen.iter().map(|c| (c["op"].as_str().unwrap(), c["path"].as_str().unwrap())).collect();
    assert_eq!(ops, [("mkdir", "/live"), ("create", "/live/a.md"), ("commit", "/live/a.md")]);
    assert_eq!(seen[2]["author"], "agent-9");
}
