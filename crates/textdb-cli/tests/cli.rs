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

#[test]
fn messages_create_paths_search_grep_and_empty_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let doc = "---\nentity_name: RFI Response Tracker\nmodified_date: 2026-02-10\n---\n# Tracker\n\nNothing here.\n\n## Changelog\n\n- 2026-02-11: renamed the tracker file\n";
    ok(textdb(&store).args(["write", "/acc/x.md", "-m", "first"]), Some(doc));

    // Search: several words are one query; each line holding a word is listed at its own line.
    let found = ok(textdb(&store).args(["search", "RFI", "tracker", "-p", "/acc"]), None).stdout;
    assert_eq!(
        found,
        "/acc/x.md:2: entity_name: RFI Response Tracker\n/acc/x.md:5: # Tracker\n/acc/x.md:11: - 2026-02-11: renamed the tracker file\n"
    );
    let phrase = ok(textdb(&store).args(["search", "\"RFI Response\""]), None).stdout;
    assert_eq!(phrase, "/acc/x.md:2: entity_name: RFI Response Tracker\n");
    let dated = ok(textdb(&store).args(["search", "2026-02"]), None).stdout;
    assert!(dated.contains("/acc/x.md:3: modified_date") && dated.contains("/acc/x.md:11: - 2026-02-11"), "{dated}");
    let none = run(textdb(&store).args(["search", "zebra"]), None);
    assert_eq!((none.status, none.stdout.as_str()), (0, ""));
    assert!(none.stderr.contains("no matches"), "{}", none.stderr);

    // grep: regular expressions, case-sensitive unless -i, or only the files.
    assert_eq!(ok(textdb(&store).args(["grep", r"modified_date: 2026-\d\d"]), None).stdout, "/acc/x.md:3: modified_date: 2026-02-10\n");
    assert_eq!(ok(textdb(&store).args(["grep", "-il", "TRACKER"]), None).stdout, "/acc/x.md\n");
    assert!(run(textdb(&store).args(["grep", "TRACKER"]), None).stderr.contains("no matches"));

    // A message on every kind of edit.
    ok(textdb(&store).args(["edit", "/acc/x.md", "--old", "Nothing here.", "--new", "Nothing yet.", "-m", "wording"]), None);
    ok(textdb(&store).args(["replace-lines", "/acc/x.md", "6", "6", "--text", "Intro.\n", "-m", "intro"]), None);
    ok(textdb(&store).args(["append", "/acc/x.md", "- 2026-02-12: more", "-m", "log"]), None);
    let history = ok(textdb(&store).args(["--json", "history", "--versions-only", "/acc/x.md"]), None).json();
    let messages: Vec<&str> = history.as_array().unwrap().iter().map(|c| c["message"].as_str().unwrap_or("")).collect();
    assert_eq!(messages, ["first", "wording", "intro", "log"]);

    // --create never replaces a file.
    let exists = run(textdb(&store).args(["write", "--create", "/acc/x.md"]), Some("again\n"));
    assert_eq!(exists.status, 6, "{}", exists.stderr);
    ok(textdb(&store).args(["write", "--create", "/acc/new.md"]), Some("new\n"));
    assert!(ok(textdb(&store).args(["cat", "/acc/x.md"]), None).stdout.contains("# Tracker"));

    // ls --paths: one path per line.
    assert_eq!(ok(textdb(&store).args(["ls", "--paths", "/acc"]), None).stdout, "/acc/new.md\n/acc/x.md\n");

    // A move or delete that empties folders removes them, unless told to keep them.
    ok(textdb(&store).args(["write", "/drafts/rsi/a.md"]), Some("a\n"));
    let moved = ok(textdb(&store).args(["mv", "/drafts/rsi/a.md", "/acc/a.md", "-m", "move drafts"]), None).stdout;
    assert!(moved.contains("removed empty folder /drafts/rsi\n") && moved.contains("removed empty folder /drafts\n"), "{moved}");
    assert_eq!(run(textdb(&store).args(["stat", "/drafts"]), None).status, 5);
    let logged = ok(textdb(&store).args(["--json", "sql", "SELECT message FROM kb_change WHERE op = 'move'"]), None).json();
    assert_eq!(logged["rows"][0]["message"], "move drafts", "{logged}");
    ok(textdb(&store).args(["write", "/tmp/one/b.md"]), Some("b\n"));
    ok(textdb(&store).args(["rm", "--keep-empty-folders", "/tmp/one/b.md"]), None);
    assert_eq!(ok(textdb(&store).args(["--json", "stat", "/tmp/one"]), None).json()["kind"], "folder");
    let deleted = ok(textdb(&store).args(["rm", "/acc/new.md"]), None).stdout;
    assert_eq!(deleted, "deleted /acc/new.md\n");
}

#[test]
fn links_resolve_and_stay_current_as_files_come_and_go() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let status = |store: &std::path::Path| -> Vec<(String, Option<String>)> {
        let rows = ok(textdb(store).args(["--json", "links", "/acc/acme.md"]), None).json();
        rows.as_array()
            .unwrap()
            .iter()
            .map(|r| (r["status"].as_str().unwrap().to_string(), r["resolved"].as_str().map(str::to_string)))
            .collect()
    };
    let s = |st: &str, to: Option<&str>| (st.to_string(), to.map(str::to_string));
    ok(textdb(&store).args(["write", "/notes/Plan.md"]), Some("# Plan\n\n## Next steps\n"));
    ok(
        textdb(&store).args(["write", "/acc/acme.md"]),
        Some("[[Plan]] [[Plan#Next steps]] [[plan#Nope]]\n[x](../notes/Plan.md) [[Missing]] ![[deck.pdf]]\n[g](https://x.y) | [[notes/Plan\\|p]] |\n"),
    );
    let plan = Some("/notes/Plan.md");
    assert_eq!(
        status(&store),
        [s("ok", plan), s("ok", plan), s("anchor-missing", plan), s("ok", plan), s("broken", None), s("not-in-store", None), s("external", None), s("ok", plan)]
    );
    assert_eq!(ok(textdb(&store).args(["--json", "backlinks", "/notes/Plan.md"]), None).json().as_array().unwrap().len(), 5);
    let broken = ok(textdb(&store).args(["links", "--broken", "/acc"]), None).stdout;
    assert_eq!(
        broken,
        "/acc/acme.md:1: [[plan#Nope]] -> /notes/Plan.md (anchor-missing)\n/acc/acme.md:2: [[Missing]] (broken)\n/acc/acme.md:2: ![[deck.pdf]] (not-in-store)\n"
    );
    // A synced directory holding the PDF: only the two real problems remain.
    let disk = tmp.path().join("vault");
    std::fs::create_dir_all(disk.join("acc")).unwrap();
    std::fs::write(disk.join("acc/deck.pdf"), b"%PDF").unwrap();
    let checked = ok(textdb(&store).args(["--json", "links", "--broken", "--dir", disk.to_str().unwrap()]), None).json();
    assert_eq!(checked.as_array().unwrap().len(), 2, "{checked}");

    // A new file resolves links that waited for it; a second file of the same name makes them ambiguous.
    ok(textdb(&store).args(["write", "/Missing.md"]), Some("here\n"));
    assert_eq!(status(&store)[4], s("ok", Some("/Missing.md")));
    ok(textdb(&store).args(["write", "/other/Plan.md"]), Some("# Other\n"));
    assert_eq!(status(&store)[0], s("ambiguous", plan));
    ok(textdb(&store).args(["rm", "/other/Plan.md"]), None);
    assert_eq!(status(&store)[0], s("ok", plan));

    // Moving the target away breaks the links written to its old name and path.
    ok(textdb(&store).args(["mv", "/notes/Plan.md", "/archive/Plan-old.md"]), None);
    let after = status(&store);
    assert_eq!((after[0].0.as_str(), after[3].0.as_str(), after[7].0.as_str()), ("broken", "broken", "broken"));
    let view = ok(textdb(&store).args(["--json", "sql", "SELECT kind, alias, status, resolved FROM links WHERE target = 'Missing'"]), None).json();
    assert_eq!(view["rows"][0]["resolved"], "/Missing.md", "{view}");

    // Links to folders are neither files nor broken.
    ok(textdb(&store).args(["write", "/acc/folders.md"]), Some("[[archive/]] [a](../archive) [[nowhere/]]\n"));
    let folders = ok(textdb(&store).args(["--json", "links", "/acc/folders.md"]), None).json();
    let statuses: Vec<&str> = folders.as_array().unwrap().iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["folder", "folder", "broken"]);
}

#[test]
fn moves_report_or_rewrite_the_links_that_pointed_at_what_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let cat = |p: &str| ok(textdb(&store).args(["cat", p]), None).stdout;
    let acme = "See [[Plan]], [[Plan#Next steps|next]] and [plan](../notes/Plan.md#next-steps).\n| [[notes/Plan\\|p]] |\n";
    let index = "[[Plan]] ![[Plan]] [p](Plan.md)\n";
    ok(textdb(&store).args(["write", "/notes/Plan.md"]), Some("# Plan\n\n## Next steps\n"));
    ok(textdb(&store).args(["write", "/acc/acme.md"]), Some(acme));
    ok(textdb(&store).args(["write", "/notes/index.md"]), Some(index));

    // By default a move lists the links it leaves behind and changes nothing.
    let moved = ok(textdb(&store).args(["mv", "/notes/Plan.md", "/archive/2026/Plan-v1.md"]), None).stdout;
    assert!(moved.contains("7 links in 2 files pointed to what moved from /notes/Plan.md and no longer do"), "{moved}");
    assert!(moved.contains("  /acc/acme.md:1: [[Plan]] (now /archive/2026/Plan-v1.md)\n"), "{moved}");
    assert_eq!((cat("/acc/acme.md"), cat("/notes/index.md")), (acme.to_string(), index.to_string()));

    // Moving it back without touching links makes them resolve again.
    let back = ok(textdb(&store).args(["mv", "--no-update-links", "/archive/2026/Plan-v1.md", "/notes/Plan.md"]), None).stdout;
    assert!(!back.contains("links"), "{back}");
    assert_eq!(ok(textdb(&store).args(["--json", "links", "--broken"]), None).json(), serde_json::json!([]));

    // --update-links rewrites each link in the style it was written, one commit per file.
    let rewrote = ok(textdb(&store).args(["mv", "--update-links", "/notes/Plan.md", "/archive/2026/Plan-v1.md"]), None).stdout;
    assert!(rewrote.contains("rewrote 7 links in 2 files"), "{rewrote}");
    assert_eq!(
        cat("/acc/acme.md"),
        "See [[Plan-v1]], [[Plan-v1#Next steps|next]] and [plan](../archive/2026/Plan-v1.md#next-steps).\n| [[archive/2026/Plan-v1\\|p]] |\n"
    );
    assert_eq!(cat("/notes/index.md"), "[[Plan-v1]] ![[Plan-v1]] [p](../archive/2026/Plan-v1.md)\n");
    assert_eq!(ok(textdb(&store).args(["--json", "links", "--broken"]), None).json(), serde_json::json!([]));
    let history = ok(textdb(&store).args(["--json", "history", "--versions-only", "/acc/acme.md"]), None).json();
    assert_eq!(history[1]["message"], "links: /notes/Plan.md -> /archive/2026/Plan-v1.md");

    // With the store setting, every move rewrites; spaces are encoded in markdown links.
    ok(textdb(&store).args(["setting", "link_updates", "rewrite"]), None);
    assert_eq!(ok(textdb(&store).args(["setting", "link_updates"]), None).stdout, "link_updates  rewrite\n");
    assert_eq!(run(textdb(&store).args(["setting", "link_updates", "sometimes"]), None).status, 6);
    ok(textdb(&store).args(["mv", "/archive/2026/Plan-v1.md", "/archive/My Plan.md"]), None);
    assert_eq!(
        cat("/acc/acme.md"),
        "See [[My Plan]], [[My Plan#Next steps|next]] and [plan](../archive/My%20Plan.md#next-steps).\n| [[archive/My Plan\\|p]] |\n"
    );

    // Deleting it lists the links that are now broken.
    let deleted = ok(textdb(&store).args(["rm", "/archive/My Plan.md"]), None).stdout;
    assert!(deleted.contains("7 links in 2 files pointed here and are now broken"), "{deleted}");
}

#[test]
fn several_line_ranges_make_one_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    ok(textdb(&store).args(["write", "/r.md"]), Some("one\ntwo\nthree\nfour\nfive\n"));

    // Numbered as in v1, in any order: one new version with one message.
    let ranges = r#"[{"from": 5, "to": 5, "text": "FIVE\n"}, {"from": 1, "to": 1, "text": "ONE\n"}, {"from": 3, "to": 2, "text": "2.5\n"}]"#;
    let w = ok(textdb(&store).args(["--json", "replace-lines", "/r.md", "-b", "1", "--stdin-json", "-m", "tidy"]), Some(ranges)).json();
    assert_eq!((w["version"].as_i64(), w["kind"].as_str()), (Some(2), Some("direct")), "{w}");
    assert_eq!(ok(textdb(&store).args(["cat", "/r.md"]), None).stdout, "ONE\ntwo\n2.5\nthree\nfour\nFIVE\n");
    let history = ok(textdb(&store).args(["--json", "history", "--versions-only", "/r.md"]), None).json();
    assert_eq!(history.as_array().unwrap().len(), 2);
    assert_eq!(history[1]["message"], "tidy");

    // Overlapping or out-of-file ranges change nothing.
    let overlap = run(textdb(&store).args(["replace-lines", "/r.md", "--stdin-json"]), Some(r#"[{"from": 2, "to": 3, "text": ""}, {"from": 3, "to": 4, "text": ""}]"#));
    assert_eq!(overlap.status, 6, "{}", overlap.stderr);
    let outside = run(textdb(&store).args(["replace-lines", "/r.md", "--stdin-json"]), Some(r#"[{"from": 1, "to": 1, "text": "x\n"}, {"from": 20, "to": 20, "text": ""}]"#));
    assert_eq!(outside.status, 6, "{}", outside.stderr);
    assert_eq!(ok(textdb(&store).args(["stat", "--json", "/r.md"]), None).json()["version"], 2);

    // Against a stale version, rebased over a commit elsewhere in the file.
    ok(textdb(&store).args(["append", "/r.md", "six"]), None);
    let stale = ok(textdb(&store).args(["--json", "replace-lines", "/r.md", "-b", "2", "--stdin-json"]), Some(r#"[{"from": 2, "to": 2, "text": "TWO\n"}, {"from": 4, "to": 4, "text": "THREE\n"}]"#)).json();
    assert_eq!(stale["kind"], "rebased", "{stale}");
    assert_eq!(ok(textdb(&store).args(["cat", "/r.md"]), None).stdout, "ONE\nTWO\n2.5\nTHREE\nfour\nFIVE\nsix\n");
}

#[test]
fn meta_changes_one_front_matter_key_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let doc = "---\r\ntitle: Acme\r\ntags:\r\n    - telco\r\n    - cvm\r\nstatus: draft # was review\r\nowner: \"[[Jonas]]\"\r\n---\r\n# Acme\r\n";
    ok(textdb(&store).args(["write", "/m.md"]), Some(doc));

    assert_eq!(ok(textdb(&store).args(["meta", "get", "/m.md", "title"]), None).stdout, "Acme\n");
    assert_eq!(ok(textdb(&store).args(["meta", "get", "/m.md", "tags"]), None).stdout, "telco\ncvm\n");
    assert_eq!(ok(textdb(&store).args(["meta", "get", "/m.md", "owner"]), None).stdout, "[[Jonas]]\n");
    assert_eq!(ok(textdb(&store).args(["--json", "meta", "get", "/m.md"]), None).json()["tags"], serde_json::json!(["telco", "cvm"]));
    assert_eq!(run(textdb(&store).args(["meta", "get", "/m.md", "missing"]), None).status, 5);

    ok(textdb(&store).args(["meta", "set", "/m.md", "status", "published"]), None);
    ok(textdb(&store).args(["meta", "set", "/m.md", "tags", "telco", "cvm", "rfi"]), None);
    ok(textdb(&store).args(["meta", "set", "/m.md", "related", "[[Acme]]", "-m", "link the account"]), None);
    ok(textdb(&store).args(["meta", "unset", "/m.md", "owner"]), None);
    assert_eq!(
        ok(textdb(&store).args(["cat", "/m.md"]), None).stdout,
        "---\r\ntitle: Acme\r\ntags:\r\n    - telco\r\n    - cvm\r\n    - rfi\r\nstatus: published\r\nrelated: \"[[Acme]]\"\r\n---\r\n# Acme\r\n"
    );
    let history = ok(textdb(&store).args(["--json", "history", "--versions-only", "/m.md"]), None).json();
    let messages: Vec<&str> = history.as_array().unwrap().iter().map(|c| c["message"].as_str().unwrap_or("")).collect();
    assert_eq!(messages[1..], ["meta set status", "meta set tags", "link the account", "meta unset owner"]);

    // Setting what is there, or removing what is not, makes no version.
    assert!(ok(textdb(&store).args(["meta", "set", "/m.md", "status", "published"]), None).stdout.contains("unchanged"));
    assert!(ok(textdb(&store).args(["meta", "unset", "/m.md", "owner"]), None).stdout.contains("unchanged"));

    // A file without front matter gets some.
    ok(textdb(&store).args(["write", "/plain.md"]), Some("# Plain\n"));
    ok(textdb(&store).args(["meta", "set", "/plain.md", "modified_date", "2026-09-14"]), None);
    assert_eq!(ok(textdb(&store).args(["cat", "/plain.md"]), None).stdout, "---\nmodified_date: 2026-09-14\n---\n# Plain\n");
    assert_eq!(ok(textdb(&store).args(["--json", "meta", "get", "/plain.md", "modified_date"]), None).json(), "2026-09-14");
}

#[test]
fn sql_reads_through_views_and_writes_only_when_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let src = tmp.path().join("src");
    corpus(&src);
    ok(textdb(&store).arg("import").arg(&src).args(["--prefix", "/docs"]), None);
    let account = "---\nentity_type: account\nstatus: active\nrelated_accounts: [globex]\n---\n# Acme\n\n## Next steps\n\nSee [Globex](globex.md) and [[initech]].\n";
    ok(textdb(&store).args(["--author", "ana", "write", "/accounts/acme.md"]), Some(account));
    let sql = |args: &[&str], stdin: Option<&str>| {
        let mut cmd = textdb(&store);
        cmd.args(["--json", "sql"]).args(args);
        run(&mut cmd, stdin)
    };

    let fm = sql(
        &["SELECT path, json_extract(data, '$.status') AS status FROM frontmatter WHERE json_extract(data, '$.entity_type') = ?", "-p", "account"],
        None,
    );
    assert_eq!(fm.status, 0, "{}", fm.stdout);
    assert_eq!(fm.json()["rows"], serde_json::json!([{ "path": "/accounts/acme.md", "status": "active" }]));
    let counted = sql(&["SELECT count(*) AS n FROM files WHERE path LIKE ?", "-p", "/docs/%"], None).json();
    assert_eq!(counted["rows"][0]["n"], 3, "{counted}");
    let sections = sql(&["SELECT heading, level FROM sections WHERE path = '/accounts/acme.md' ORDER BY line_from"], None).json();
    assert_eq!(sections["rows"][1], serde_json::json!({ "heading": "Acme / Next steps", "level": 2 }), "{sections}");
    let links = sql(&["SELECT target FROM links WHERE path = '/accounts/acme.md'"], None).json();
    assert!(links["row_count"].as_i64().unwrap() >= 1, "{links}");

    // The textdb functions read too; the statement can come from stdin.
    let listed = sql(&[], Some("SELECT name, nwords FROM textdb_ls('/docs') ORDER BY name;\n")).json();
    assert_eq!(listed["columns"], serde_json::json!(["name", "nwords"]), "{listed}");
    let content = sql(&["SELECT length(textdb_content('/accounts/acme.md')) AS n"], None).json();
    assert_eq!(content["rows"][0]["n"], account.len());
    let text = ok(textdb(&store).args(["sql", "SELECT path, nlines FROM files ORDER BY path"]), None).stdout;
    assert!(text.starts_with("path") && text.contains("/accounts/acme.md") && text.trim_end().ends_with("(4 rows)"), "{text}");

    // Changes need --write and go through the textdb functions, under the author's name.
    let refused = sql(&["SELECT textdb_append('/accounts/acme.md', '- call back')"], None);
    assert_eq!(refused.status, 6, "{}", refused.stdout);
    assert!(refused.stdout.contains("--write"), "{}", refused.stdout);
    assert_eq!(sql(&["DELETE FROM kb WHERE path = '/accounts/acme.md'"], None).status, 6);
    let wrote = run(
        textdb(&store).args([
            "--json",
            "--author",
            "claude",
            "sql",
            "--write",
            "SELECT textdb_append(path, '- call back', :author) AS version FROM files WHERE path = ?",
            "-p",
            "/accounts/acme.md",
        ]),
        None,
    );
    assert_eq!(wrote.status, 0, "{}", wrote.stdout);
    let wrote = wrote.json();
    assert_eq!((wrote["rows"][0]["version"].as_i64(), wrote["store_changes"].as_i64()), (Some(2), Some(1)), "{wrote}");
    let history = ok(textdb(&store).args(["--json", "history", "/accounts/acme.md"]), None).stdout;
    assert!(history.contains("\"claude\""), "{history}");
    let internal = sql(&["--write", "DELETE FROM kb_node"], None);
    assert_eq!(internal.status, 6);
    assert!(internal.stdout.contains("kb_node"), "{}", internal.stdout);
    assert_eq!(sql(&["SELECT 1; SELECT 2"], None).status, 6);
    assert_eq!(sql(&["SELECT ?", "-p", "a", "-p", "b"], None).status, 6);

    // A writing statement is all or nothing: the edit that fails on the second file undoes the first.
    ok(textdb(&store).args(["write", "/notes/a-draft.md"]), Some("status: draft\n"));
    ok(textdb(&store).args(["write", "/notes/b-other.md"]), Some("no status\n"));
    let partial = sql(
        &["--write", "SELECT textdb_edit(path, 'status: draft', 'status: done', :author) FROM files WHERE path LIKE '/notes/%' ORDER BY path"],
        None,
    );
    assert_eq!(partial.status, 6, "{}", partial.stdout);
    assert_eq!(ok(textdb(&store).args(["cat", "/notes/a-draft.md"]), None).stdout, "status: draft\n");
}

#[test]
fn sql_bulk_edits_dry_runs_batches_revert_and_formats() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let a = "---\nentity_name: '[''Account Name'']'\nmeeting_date: \"2025-11-[DD]\"  # UPDATE when known\n---\n# Acme teo-group\n\n- item one\n- item two\nAcme Acme\n";
    ok(textdb(&store).args(["write", "/p/a.md"]), Some(a));
    ok(textdb(&store).args(["write", "/p/b.md"]), Some("Acme once\n"));
    ok(textdb(&store).args(["write", "/p/old/c.md"]), Some("gone soon\n"));
    let sql = |args: &[&str]| run(textdb(&store).args(["--author", "claude", "sql"]).args(args), None);

    // Front matter values as YAML means them: comment gone, '' unescaped.
    let fm = sql(&["--format", "tsv", "SELECT json_extract(data, '$.entity_name') AS name, json_extract(data, '$.meeting_date') AS d FROM frontmatter"]);
    assert_eq!(fm.stdout, "name\td\n['Account Name']\t2025-11-[DD]\n", "{}", fm.stderr);

    // Path helpers, TSV and one value per line.
    let files = sql(&["--format", "tsv", "SELECT path, dir, depth, ext FROM files ORDER BY path"]);
    assert_eq!(files.stdout, "path\tdir\tdepth\text\n/p/a.md\t/p\t2\tmd\n/p/b.md\t/p\t2\tmd\n/p/old/c.md\t/p/old\t3\tmd\n");
    assert_eq!(sql(&["--format", "lines", "SELECT parent FROM folders WHERE path = '/p/old'"]).stdout, "/p\n");
    assert_eq!(sql(&["--format", "lines", "SELECT name FROM files ORDER BY name"]).stdout, "a.md\nb.md\nc.md\n");
    assert_eq!(sql(&["--format", "lines", "SELECT name, path FROM files"]).status, 6);

    // A hyphenated term matches as the phrase it is.
    assert_eq!(sql(&["--format", "lines", "SELECT path FROM textdb_search('teo-group', '/')"]).stdout, "/p/a.md\n");

    // A dry run shows the diffs and writes nothing.
    let replace_acme = "SELECT textdb_replace(path, 'Acme', 'Globex', NULL, :author) AS v FROM files \
                        WHERE path LIKE '/p/%' AND instr(textdb_content(path), 'Acme') > 0 ORDER BY path";
    let preview = sql(&["--write", "--dry-run", replace_acme]);
    assert_eq!(preview.status, 0, "{}", preview.stderr);
    assert!(preview.stdout.contains("dry run: 2 changes") && preview.stdout.contains("-Acme once") && preview.stdout.contains("+Globex once"), "{}", preview.stdout);
    assert_eq!(ok(textdb(&store).args(["cat", "/p/b.md"]), None).stdout, "Acme once\n");
    // A count that does not match undoes the statement.
    let wrong = sql(&["--write", "SELECT textdb_replace('/p/a.md', 'Acme', 'Globex', 2, :author)"]);
    assert_eq!(wrong.status, 6, "{}", wrong.stdout);
    assert!(wrong.stderr.contains("expected 2 occurrences") && wrong.stderr.contains("found 3"), "{}", wrong.stderr);

    // Several replacements, a move and a delete from a file: one batch, all or nothing.
    let file = tmp.path().join("bulk.sql");
    std::fs::write(
        &file,
        "SELECT textdb_replace_many('/p/a.md', '[[\"Acme\", \"Globex\", 3], {\"old\": \"- item\", \"new\": \"- point\"}]', :author) AS a,\n\
         textdb_move('/p/b.md', '/p/b2.md', :author) AS m,\n\
         textdb_delete('/p/old', :author) AS d;\n",
    )
    .unwrap();
    let wrote = ok(textdb(&store).args(["--json", "--author", "claude", "sql", "--write", "-f"]).arg(&file), None).json();
    assert_eq!(wrote["store_changes"], 3, "{wrote}");
    let batch = wrote["batch"].as_str().unwrap().to_string();
    assert_eq!(ok(textdb(&store).args(["--json", "history", "--versions-only", "/p/a.md"]), None).json().as_array().unwrap().len(), 2);
    assert!(ok(textdb(&store).args(["cat", "/p/a.md"]), None).stdout.contains("# Globex teo-group\n\n- point one\n- point two\nGlobex Globex\n"));
    assert_eq!(sql(&["--format", "lines", "SELECT path FROM commits WHERE batch = ?", "-p", &batch]).stdout, "/p/a.md\n");

    // A value starting with "- " works as --old.
    ok(textdb(&store).args(["edit", "/p/a.md", "--old", "- point two", "--new", "- point 2"]), None);

    // a.md changed since the batch: the revert refuses and changes nothing.
    let refused = run(textdb(&store).args(["revert-batch", &batch]), None);
    assert_eq!(refused.status, 6, "{}", refused.stdout);
    assert!(refused.stderr.contains("/p/a.md") && refused.stderr.contains("--skip-changed"), "{}", refused.stderr);
    ok(textdb(&store).args(["stat", "/p/b2.md"]), None);
    let dry = ok(textdb(&store).args(["--json", "revert-batch", "--skip-changed", "--dry-run", &batch]), None).json();
    assert_eq!(dry["moved_back"], serde_json::json!([{ "from": "/p/b2.md", "to": "/p/b.md" }]), "{dry}");
    assert_eq!(run(textdb(&store).args(["stat", "/p/b.md"]), None).status, 5);
    let reverted = ok(textdb(&store).args(["revert-batch", "--skip-changed", &batch]), None).stdout;
    assert!(reverted.contains("moved back /p/b2.md -> /p/b.md") && reverted.contains("recreated /p/old/c.md") && reverted.contains("skipped: /p/a.md"), "{reverted}");
    assert_eq!(ok(textdb(&store).args(["cat", "/p/b.md"]), None).stdout, "Acme once\n");
    assert_eq!(ok(textdb(&store).args(["cat", "/p/old/c.md"]), None).stdout, "gone soon\n");

    // A clean batch goes back exactly, and the table output names the batch.
    let second = sql(&["--write", "SELECT textdb_replace('/p/b.md', 'once', 'twice', 1, :author) AS v"]);
    assert_eq!(second.status, 0, "{}", second.stderr);
    let id = second.stdout.lines().find_map(|l| l.strip_prefix("batch ")).and_then(|l| l.split(':').next()).unwrap().to_string();
    assert!(second.stdout.contains(&format!("textdb revert-batch {id}")), "{}", second.stdout);
    let undone = ok(textdb(&store).args(["--json", "revert-batch", &id]), None).json();
    assert_eq!(undone["restored"][0]["path"], "/p/b.md", "{undone}");
    assert_eq!(ok(textdb(&store).args(["cat", "/p/b.md"]), None).stdout, "Acme once\n");
    assert_eq!(run(textdb(&store).args(["revert-batch", "20000101-000000-dead"]), None).status, 5);
}

#[test]
fn assets_push_pull_verify_links_and_gitignore() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let vault = tmp.path().join("vault");
    let bucket = tmp.path().join("bucket");
    let config = tmp.path().join("config");
    for d in ["notes", "img", "docs", "data"] {
        std::fs::create_dir_all(vault.join(d)).unwrap();
    }
    std::fs::create_dir_all(&bucket).unwrap();
    std::fs::write(vault.join("notes/a.md"), "![[arch.png]]\n[deck](../docs/deck.pdf)\n").unwrap();
    std::fs::write(vault.join("img/arch.png"), [137u8, 80, 78, 71, 0, 1]).unwrap();
    std::fs::write(vault.join("docs/deck.pdf"), b"%PDF-1.4 one").unwrap();
    std::fs::write(vault.join("data/x.dat"), b"plain text").unwrap();
    std::fs::write(vault.join(".gitattributes"), "*.dat textdb=asset\n").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let status = |args: &[&str]| ok(&mut t(&[&["--json", "assets", "status"], args].concat()), None).json();
    let first = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert_eq!(first.status, 0, "{}", first.stderr);

    // Before anything is pushed: three new assets, links to them not in the store, no asset store.
    assert_eq!(status(&[])["counts"], serde_json::json!({ "new": 3 }));
    assert_eq!(ok(&mut t(&["--json", "links", "/notes/a.md"]), None).json()[0]["status"], "not-in-store");
    assert_eq!(run(&mut t(&["assets", "push"]), None).status, 6);
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);

    // Push: the bytes to the asset store, the pointers to the store and next to the files.
    let pushed = ok(&mut t(&["--json", "assets", "push", "-m", "first assets"]), None).json();
    assert_eq!(pushed["pushed"].as_array().unwrap().len(), 3, "{pushed}");
    assert_eq!(std::fs::read(bucket.join("img/arch.png")).unwrap(), [137u8, 80, 78, 71, 0, 1]);
    let pointer = std::fs::read_to_string(vault.join("img/arch.png.tdbasset")).unwrap();
    assert!(pointer.starts_with("textdb-asset: 1\nid: ") && pointer.contains("store: team\n") && pointer.contains("type: image/png\n"), "{pointer}");
    assert_eq!(ok(&mut t(&["cat", "/img/arch.png.tdbasset"]), None).stdout, pointer);
    let history = ok(&mut t(&["--json", "history", "--versions-only", "/docs/deck.pdf.tdbasset"]), None).json();
    assert_eq!(history[0]["message"], "first assets");
    assert_eq!(status(&[])["counts"], serde_json::json!({ "ok": 3 }));

    // Links reach the assets, shown by their own paths.
    let links = ok(&mut t(&["--json", "links", "/notes/a.md"]), None).json();
    assert_eq!(
        (links[0]["status"].as_str(), links[0]["resolved"].as_str(), links[0]["asset"].as_bool()),
        (Some("ok"), Some("/img/arch.png"), Some(true)),
        "{links}"
    );
    assert_eq!(ok(&mut t(&["--json", "backlinks", "/docs/deck.pdf"]), None).json().as_array().unwrap().len(), 1);
    let again = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert_eq!(again.status, 0, "the pointers are alike on both sides: {}", again.stdout);

    // A changed image is pushed under the same id; the bytes it replaced go to the store's trash.
    std::fs::write(vault.join("img/arch.png"), [137u8, 80, 78, 71, 0, 2, 3]).unwrap();
    assert_eq!(status(&["/img"])["assets"][0]["state"], "modified");
    ok(&mut t(&["assets", "push", "/img"]), None);
    let pointer2 = std::fs::read_to_string(vault.join("img/arch.png.tdbasset")).unwrap();
    let id = |p: &str| p.lines().find_map(|l| l.strip_prefix("id: ")).unwrap().to_string();
    assert_eq!(id(&pointer2), id(&pointer));
    assert!(pointer2.contains("size: 7\n"), "{pointer2}");
    assert!(bucket.join(".textdb-trash").is_dir());

    // A file missing here: not pulled, a broken link for this directory, then pulled and checked.
    std::fs::remove_file(vault.join("docs/deck.pdf")).unwrap();
    assert_eq!(status(&[])["counts"], serde_json::json!({ "not-pulled": 1, "ok": 2 }));
    let broken = ok(&mut t(&["links", "--broken", "--dir", dir]), None).stdout;
    assert!(broken.contains("[](../docs/deck.pdf) -> /docs/deck.pdf (not-pulled)"), "{broken}");
    let pulled = ok(&mut t(&["--json", "assets", "pull", "--linked-from", "/notes"]), None).json();
    assert_eq!(pulled["pulled"].as_array().unwrap().len(), 1, "{pulled}");
    assert_eq!(std::fs::read(vault.join("docs/deck.pdf")).unwrap(), b"%PDF-1.4 one");
    assert_eq!(ok(&mut t(&["--json", "assets", "verify"]), None).json()["problems"], 0);

    // Bytes damaged in the asset store: verify fails, and pull does not put them in place.
    std::fs::write(bucket.join("data/x.dat"), b"damaged").unwrap();
    let bad = run(&mut t(&["--json", "assets", "verify"]), None);
    assert_eq!(bad.status, 1, "{}", bad.stdout);
    assert!(bad.stdout.contains("\"asset_store\":\"differs\""), "{}", bad.stdout);
    std::fs::remove_file(vault.join("data/x.dat")).unwrap();
    let refused = run(&mut t(&["assets", "pull"]), None);
    assert_eq!(refused.status, 1, "{}", refused.stdout);
    assert!(refused.stdout.contains("other bytes") && !vault.join("data/x.dat").exists(), "{}", refused.stdout);

    // Moving an asset's pointer rewrites the links to the asset.
    ok(&mut t(&["mv", "--update-links", "/docs/deck.pdf.tdbasset", "/archive/deck.pdf.tdbasset"]), None);
    assert_eq!(ok(&mut t(&["cat", "/notes/a.md"]), None).stdout, "![[arch.png]]\n[deck](../archive/deck.pdf)\n");

    // .gitignore keeps the assets out of git and their pointers in.
    std::fs::write(vault.join(".gitignore"), "target/\n").unwrap();
    ok(&mut t(&["assets", "gitignore"]), None);
    let gi = std::fs::read_to_string(vault.join(".gitignore")).unwrap();
    assert!(gi.starts_with("# BEGIN textdb assets") && gi.ends_with("\ntarget/\n"), "{gi}");
    assert!(gi.contains("\n*.[pP][nN][gG]\n") && gi.contains("\n*.dat\n") && gi.contains("\n!*.tdbasset\n"), "{gi}");
    assert!(ok(&mut t(&["assets", "gitignore"]), None).stdout.contains("up to date"));

    // Where this computer reaches the asset store.
    ok(&mut t(&["assets", "stores", "--bind", &format!("team={}", bucket.display())]), None);
    let stores = ok(&mut t(&["--json", "assets", "stores"]), None).json();
    assert_eq!((stores[0]["reachable"].as_bool(), stores[0]["bound_to"].as_str()), (Some(true), bucket.to_str()), "{stores}");
}

#[test]
fn assets_between_vaults_conflicts_outdated_case_and_junk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (v1, v2, bucket, config) = (tmp.path().join("v1"), tmp.path().join("v2"), tmp.path().join("bucket"), tmp.path().join("config"));
    for d in [&v1, &v2] {
        std::fs::create_dir_all(d.join("img")).unwrap();
    }
    std::fs::create_dir_all(&bucket).unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let (d1, d2) = (v1.to_str().unwrap(), v2.to_str().unwrap());
    let state = |dir: &str| ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json();
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);

    // Vault 1 publishes an image; vault 2 has other bytes under that name and never had vault 1's.
    std::fs::write(v1.join("img/x.png"), b"\x89PNG alice").unwrap();
    std::fs::write(v2.join("img/x.png"), b"\x89PNG bob").unwrap();
    ok(&mut t(&["sync", "/", d1]), None);
    ok(&mut t(&["assets", "push", "--dir", d1]), None);
    ok(&mut t(&["sync", "/", d2]), None);
    assert!(v2.join("img/x.png.tdbasset").exists());
    let s2 = state(d2);
    assert_eq!(s2["assets"][0]["state"], "conflict", "{s2}");
    let refused = run(&mut t(&["assets", "push", "--dir", d2]), None);
    assert_eq!(refused.status, 3, "{}", refused.stdout);
    assert_eq!(std::fs::read(bucket.join("img/x.png")).unwrap(), b"\x89PNG alice");
    assert!(ok(&mut t(&["assets", "pull", "--dir", d2]), None).stdout.contains("kept"));
    assert_eq!(std::fs::read(v2.join("img/x.png")).unwrap(), b"\x89PNG bob");
    ok(&mut t(&["assets", "push", "--force", "--dir", d2]), None);
    assert_eq!(std::fs::read(bucket.join("img/x.png")).unwrap(), b"\x89PNG bob");

    // Vault 1 still has what it pushed: outdated; pull replaces it and keeps its copy in the vault's trash.
    assert_eq!(state(d1)["assets"][0]["state"], "outdated");
    ok(&mut t(&["assets", "pull", "--dir", d1]), None);
    assert_eq!(std::fs::read(v1.join("img/x.png")).unwrap(), b"\x89PNG bob");
    assert!(v1.join(".textdb/trash").is_dir());
    ok(&mut t(&["sync", "/", d1]), None);
    assert_eq!(state(d1)["counts"], serde_json::json!({ "ok": 1 }));

    // A name that differs only in case is the same asset; junk, and text marked -text, are not assets.
    std::fs::rename(v1.join("img/x.png"), v1.join("img/X.png")).unwrap();
    std::fs::write(v1.join(".gitattributes"), "* -text\n").unwrap();
    std::fs::write(v1.join("data.json"), b"{}").unwrap();
    for junk in ["img/._x.png", "kb.db.bak", "notes.tmp"] {
        std::fs::write(v1.join(junk), b"\0junk").unwrap();
    }
    std::fs::create_dir_all(v1.join(".obsidian/plugins/p")).unwrap();
    std::fs::write(v1.join(".obsidian/plugins/p/main.wasm"), b"\0asm").unwrap();
    let s1 = state(d1);
    assert_eq!(s1["counts"], serde_json::json!({ "ok": 1 }), "{s1}");

    // A pointer deleted in the store is deleted on disk by the next sync, not taken in again.
    ok(&mut t(&["rm", "/img/x.png.tdbasset"]), None);
    let synced = run(&mut t(&["--json", "sync", "/", d2]), None);
    assert_eq!(synced.status, 0, "{}", synced.stdout);
    assert!(!v2.join("img/x.png.tdbasset").exists());
    assert_eq!(run(&mut t(&["stat", "/img/x.png.tdbasset"]), None).status, 5);

    // A directory synced with a folder below the top: its assets are found where they are.
    let v3 = tmp.path().join("v3");
    std::fs::create_dir_all(&v3).unwrap();
    std::fs::write(v3.join("a.md"), "![[z.png]]\n").unwrap();
    std::fs::write(v3.join("z.png"), b"\x89PNG z").unwrap();
    let d3 = v3.to_str().unwrap();
    ok(&mut t(&["sync", "/p3", d3]), None);
    ok(&mut t(&["assets", "push", "--dir", d3]), None);
    assert_eq!(ok(&mut t(&["--json", "links", "/p3", "--broken", "--dir", d3]), None).json(), serde_json::json!([]));
    assert_eq!(run(&mut t(&["assets", "status", "/elsewhere", "--dir", d3]), None).status, 6);
    assert_eq!(run(&mut t(&["assets", "push", "/p3", "/elsewhere", "--dir", d3]), None).status, 6);
}

#[test]
fn assets_push_keeps_bytes_in_use_and_records_only_what_it_wrote() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (v, w, bucket, config) = (tmp.path().join("v"), tmp.path().join("w"), tmp.path().join("bucket"), tmp.path().join("config"));
    for d in [v.join("img"), w.clone(), bucket.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let d = v.to_str().unwrap();
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    std::fs::write(v.join("img/a.png"), b"\x89PNG one").unwrap();
    ok(&mut t(&["sync", "/", d]), None);
    ok(&mut t(&["assets", "push", "--dir", d]), None);

    // A pointer moved in the store still names the bytes where they were put: a new file pushed
    // under the old name goes next to them rather than replacing them.
    ok(&mut t(&["mv", "/img/a.png.tdbasset", "/old/a.png.tdbasset"]), None);
    ok(&mut t(&["sync", "/", d]), None);
    assert_eq!(std::fs::read(v.join("old/a.png")).unwrap(), b"\x89PNG one", "sync carries the file along");
    std::fs::create_dir_all(v.join("img")).unwrap();
    std::fs::write(v.join("img/a.png"), b"\x89PNG two").unwrap();
    ok(&mut t(&["assets", "push", "--dir", d]), None);
    assert_eq!(std::fs::read(bucket.join("img/a.png")).unwrap(), b"\x89PNG one");
    std::fs::remove_file(v.join("old/a.png")).unwrap();
    ok(&mut t(&["assets", "pull", "--dir", d]), None);
    assert_eq!(std::fs::read(v.join("old/a.png")).unwrap(), b"\x89PNG one");
    assert_eq!(ok(&mut t(&["--json", "assets", "verify", "--dir", d]), None).json()["problems"], 0);

    // A pointer push cannot write here is not recorded as synced, so the next sync writes it.
    std::fs::write(v.join("img/a.png"), b"\x89PNG three").unwrap();
    let on_disk = v.join("img/a.png.tdbasset");
    let set_readonly = |yes: bool| {
        let mut p = std::fs::metadata(&on_disk).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(yes);
        std::fs::set_permissions(&on_disk, p).unwrap();
    };
    set_readonly(true);
    let failed = run(&mut t(&["assets", "push", "--dir", d]), None);
    set_readonly(false);
    assert_eq!(failed.status, 1, "{}", failed.stdout);
    ok(&mut t(&["sync", "/", d]), None);
    assert_eq!(std::fs::read_to_string(&on_disk).unwrap(), ok(&mut t(&["cat", "/img/a.png.tdbasset"]), None).stdout);
    assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", d]), None).json()["counts"], serde_json::json!({ "ok": 2 }));

    // A pointer deleted in the store is not brought back by pushing a changed file.
    ok(&mut t(&["rm", "/img/a.png.tdbasset"]), None);
    std::fs::write(v.join("img/a.png"), b"\x89PNG four").unwrap();
    let refused = run(&mut t(&["assets", "push", "--dir", d]), None);
    assert_eq!(refused.status, 3, "{}", refused.stdout);
    assert!(refused.stdout.contains("deleted in the store"), "{}", refused.stdout);
    assert_eq!(run(&mut t(&["stat", "/img/a.png.tdbasset"]), None).status, 5);

    // Push records its pointers in the base of the folder it pushed to, and leaves the others alone.
    let dw = w.to_str().unwrap();
    std::fs::write(w.join("a.md"), "a\n").unwrap();
    ok(&mut t(&["sync", "/w1", dw]), None);
    ok(&mut t(&["sync", "/w2", dw]), None);
    std::fs::write(w.join("z.png"), b"\x89PNG z").unwrap();
    ok(&mut t(&["assets", "push", "--dir", dw]), None);
    let s = ok(&mut t(&["--json", "assets", "status", "--dir", dw]), None).json();
    assert_eq!((s["prefix"].as_str(), &s["counts"]), (Some("/w2"), &serde_json::json!({ "ok": 1 })), "{s}");
    let other = run(&mut t(&["--json", "sync", "--dry-run", "/w1", dw]), None).json();
    assert_eq!(other["to_textdb"]["new"], serde_json::json!(["z.png.tdbasset"]), "{other}");

    // One push that takes a moved pointer's bytes for a new asset does not then replace them for
    // the moved pointer's own change.
    let m = tmp.path().join("m");
    std::fs::create_dir_all(m.join("img")).unwrap();
    let dm = m.to_str().unwrap();
    std::fs::write(m.join("img/a.png"), b"\x89PNG A").unwrap();
    ok(&mut t(&["sync", "/m", dm]), None);
    ok(&mut t(&["assets", "push", "--dir", dm]), None);
    ok(&mut t(&["mv", "/m/img/a.png.tdbasset", "/m/img/b.png.tdbasset"]), None);
    ok(&mut t(&["sync", "/m", dm]), None);
    if !m.join("img/a.png").exists() {
        std::fs::write(m.join("img/a.png"), b"\x89PNG A").unwrap();
    }
    std::fs::write(m.join("img/b.png"), b"\x89PNG B2").unwrap();
    let _ = run(&mut t(&["assets", "push", "--dir", dm, "--force"]), None);
    assert_eq!(ok(&mut t(&["--json", "assets", "verify", "--dir", dm]), None).json()["problems"], 0);

    // A vault inside its asset store's folder is refused.
    let inner = bucket.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    assert_eq!(run(&mut t(&["assets", "status", "/inner", "--dir", inner.to_str().unwrap()]), None).status, 6);

    // Items that differ only in case are the same file on Windows and macOS: a push never
    // replaces bytes another pointer names under another case.
    let c = tmp.path().join("c");
    std::fs::create_dir_all(c.join("img")).unwrap();
    let dc = c.to_str().unwrap();
    std::fs::write(c.join("img/Logo.png"), b"\x89PNG L1").unwrap();
    ok(&mut t(&["sync", "/c", dc]), None);
    ok(&mut t(&["assets", "push", "--dir", dc]), None);
    ok(&mut t(&["mv", "/c/img/Logo.png.tdbasset", "/c/img/x.png.tdbasset"]), None);
    ok(&mut t(&["sync", "/c", dc]), None);
    let _ = std::fs::remove_file(c.join("img/Logo.png"));
    std::fs::write(c.join("img/logo.png"), b"\x89PNG L1").unwrap();
    ok(&mut t(&["assets", "push", "--dir", dc]), None);
    std::fs::write(c.join("img/logo.png"), b"\x89PNG L2").unwrap();
    ok(&mut t(&["assets", "push", "--dir", dc]), None);
    let _ = std::fs::remove_file(c.join("img/x.png"));
    ok(&mut t(&["assets", "pull", "--dir", dc]), None);
    assert_eq!(std::fs::read(c.join("img/x.png")).unwrap(), b"\x89PNG L1");
    assert_eq!(ok(&mut t(&["--json", "assets", "verify", "--dir", dc]), None).json()["problems"], 0);

    // The .gitignore block leaves documents under a `binary` rule to git, and names assets only
    // their bytes show.
    std::fs::create_dir_all(v.join("assets")).unwrap();
    std::fs::write(v.join("assets/readme.md"), "# r\n").unwrap();
    std::fs::write(v.join("assets/pic.raw"), b"raw").unwrap();
    std::fs::write(v.join("data.bin"), b"\0\x01").unwrap();
    std::fs::create_dir_all(v.join("raw")).unwrap();
    std::fs::write(v.join("raw/readme.md"), "# raw\n").unwrap();
    std::fs::create_dir_all(v.join("docs/photos.png")).unwrap();
    std::fs::write(v.join("docs/photos.png/notes.md"), "# p\n").unwrap();
    std::fs::write(v.join(".gitattributes"), "assets/* binary\nraw binary\ntrash binary\n").unwrap();
    ok(&mut t(&["assets", "gitignore", "--dir", d]), None);
    let gi = std::fs::read_to_string(v.join(".gitignore")).unwrap();
    assert!(gi.contains("\n/.textdb/trash/\n") && gi.contains("\n!/assets/readme.md\n") && gi.contains("\n/data.bin\n"), "{gi}");
    if has_git() {
        git(&v, &["init", "-q"]);
        let ignored = |rel: &str| Command::new("git").arg("-C").arg(&v).args(["check-ignore", "-q", rel]).status().unwrap().success();
        for (rel, want) in [
            ("assets/readme.md", false),
            (".textdb/trash/old.md", true),
            (".gitignore", false),
            ("raw/readme.md", false),
            ("docs/photos.png/notes.md", false),
            ("docs/photos.png/q.png.tdbasset", false),
            ("assets/pic.raw", true),
            ("data.bin", true),
            (".textdb/trash/x/img/a.png", true),
            ("img/a.png", true),
            ("img/a.png.tdbasset", false),
            (".gitattributes", false),
        ] {
            assert_eq!(ignored(rel), want, "{rel}\n{gi}");
        }
    }
}

#[test]
fn assets_migrate_from_git_moves_tracked_binaries_out_of_git() {
    if !has_git() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (v, bucket, config) = (tmp.path().join("v"), tmp.path().join("bucket"), tmp.path().join("config"));
    std::fs::create_dir_all(v.join("img")).unwrap();
    std::fs::create_dir_all(&bucket).unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let d = v.to_str().unwrap();
    git(&v, &["init", "-q"]);
    git(&v, &["config", "user.email", "t@example.com"]);
    git(&v, &["config", "user.name", "T"]);
    std::fs::write(v.join("a.md"), "![[x.png]]\n").unwrap();
    std::fs::write(v.join("img/x.png"), b"\x89PNG x").unwrap();
    std::fs::write(v.join("img/y.pdf"), b"%PDF-1.4 y").unwrap();
    git(&v, &["add", "-A"]);
    git(&v, &["commit", "-q", "-m", "start"]);
    ok(&mut t(&["sync", "/", d]), None);
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);

    // A dry run lists what would move and changes nothing.
    let dry = ok(&mut t(&["--json", "assets", "migrate-from-git", "--dir", d, "--dry-run"]), None).json();
    assert_eq!(dry["to_push"], serde_json::json!(["/img/x.png", "/img/y.pdf"]), "{dry}");
    assert!(git(&v, &["ls-files"]).lines().any(|l| l == "img/x.png"));
    assert!(!bucket.join("img/x.png").exists());

    // Something staged already: refused, so the migration's commit holds the migration only.
    std::fs::write(v.join("b.md"), "b\n").unwrap();
    git(&v, &["add", "b.md"]);
    assert_eq!(run(&mut t(&["assets", "migrate-from-git", "--dir", d]), None).status, 6);
    git(&v, &["reset", "-q"]);

    let done = ok(&mut t(&["--json", "assets", "migrate-from-git", "--dir", d, "-m", "binaries out"]), None).json();
    assert_eq!(done["migrated"], serde_json::json!(["/img/x.png", "/img/y.pdf"]), "{done}");
    let tracked = git(&v, &["ls-files"]);
    let tracked: Vec<&str> = tracked.lines().collect();
    for (rel, want) in [("img/x.png", false), ("img/y.pdf", false), ("img/x.png.tdbasset", true), ("img/y.pdf.tdbasset", true), (".gitignore", true), ("a.md", true)] {
        assert_eq!(tracked.contains(&rel), want, "{rel} in {tracked:?}");
    }
    assert_eq!(std::fs::read(v.join("img/x.png")).unwrap(), b"\x89PNG x", "the files stay on disk");
    assert_eq!(std::fs::read(bucket.join("img/y.pdf")).unwrap(), b"%PDF-1.4 y");
    assert!(git(&v, &["log", "-1", "--format=%s"]).starts_with("binaries out:"));
    assert_eq!(git(&v, &["status", "--porcelain"]), "?? b.md", "nothing else is left over");
    assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", d]), None).json()["counts"], serde_json::json!({ "ok": 2 }));

    // Run again: nothing git tracks is a binary any more.
    let again = ok(&mut t(&["--json", "assets", "migrate-from-git", "--dir", d]), None).json();
    assert_eq!((again["tracked_assets"].as_u64(), &again["commit"]), (Some(0), &serde_json::Value::Null), "{again}");
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
fn sync_stops_when_its_include_rules_changed_and_honours_textdbignore() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    for d in [".claude", "drafts"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    for (rel, text) in [("a.md", "a\n"), ("notes.txt", "t\n"), (".claude/rules.md", "r\n"), ("drafts/x.md", "x\n"), (".textdbignore", "drafts/\n")] {
        std::fs::write(dir.join(rel), text).unwrap();
    }
    let sync = |extra: &[&str]| {
        let mut cmd = textdb(&store);
        cmd.args(["--json", "sync"]).args(extra).arg("/").arg(&dir);
        run(&mut cmd, None)
    };

    // Markdown only at first; .textdbignore keeps drafts/ out.
    let first = sync(&["--ext", "md"]);
    assert_eq!(first.status, 0, "{}", first.stderr);
    assert_eq!(first.json()["to_textdb"]["new"], serde_json::json!([".claude/rules.md", "a.md"]));

    // Wider extensions would take in notes.txt: the sync stops and lists it.
    let wider = sync(&[]);
    assert_eq!(wider.status, 6, "{}", wider.stdout);
    let w = wider.json();
    assert_eq!((w["stopped_by_rules"].as_bool(), &w["rules"]["newly_included"]), (Some(true), &serde_json::json!(["notes.txt"])));
    assert_eq!(run(textdb(&store).args(["stat", "/notes.txt"]), None).status, 5);
    let accepted = sync(&["--accept-rules"]);
    assert_eq!(accepted.status, 0, "{}", accepted.stderr);
    assert_eq!(accepted.json()["to_textdb"]["new"], serde_json::json!(["notes.txt"]));
    let quiet = sync(&[]).json();
    assert!(quiet.get("rules").is_none(), "{quiet}");

    // A base saved by a build that recorded no rules skipped hidden folders: files in them are newly included.
    let conn = rusqlite::Connection::open(&store).unwrap();
    conn.execute("UPDATE kb_sync SET rules = NULL", []).unwrap();
    drop(conn);
    std::fs::create_dir_all(dir.join(".beads")).unwrap();
    std::fs::write(dir.join(".beads/README.md"), "b\n").unwrap();
    let legacy = run(textdb(&store).args(["sync", "/"]).arg(&dir), None);
    assert_eq!(legacy.status, 6, "{}", legacy.stdout);
    assert!(legacy.stdout.contains("not recorded (an older build") && legacy.stdout.contains("newly included  .beads/README.md"), "{}", legacy.stdout);
    assert!(legacy.stderr.contains("--accept-rules"), "{}", legacy.stderr);
}

#[test]
fn sync_carries_untracked_files_with_a_moved_folder_and_reports_what_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    for d in ["proj/sub", "proj/img", "other", "empty/deeper", "keep/node_modules"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    for (rel, text) in [
        ("proj/a.md", "a\n"),
        ("proj/sub/b.md", "b\n"),
        ("proj/data.json", "{}\n"),
        ("proj/img/x.png", "png"),
        ("other/c.md", "c\n"),
        ("other/deck.pdf", "%PDF"),
        ("keep/node_modules/m.js", "m"),
    ] {
        std::fs::write(dir.join(rel), text).unwrap();
    }
    let sync = |extra: &[&str]| {
        let mut cmd = textdb(&store);
        cmd.args(["--json", "sync"]).args(extra).arg("/").arg(&dir);
        let o = run(&mut cmd, None);
        assert_eq!(o.status, 0, "{}\n{}", o.stdout, o.stderr);
        o.json()
    };
    sync(&[]);

    // The store says what a move or delete leaves on disk.
    let moved = ok(textdb(&store).args(["mv", "/proj", "/archive/proj"]), None).stdout;
    assert!(moved.contains("also holds 2 files textdb does not track (1 json, 1 png); the next sync moves them"), "{moved}");
    let deleted = ok(textdb(&store).args(["rm", "/other"]), None).stdout;
    assert!(deleted.contains("also holds 1 file textdb does not track (1 pdf); they stay on disk"), "{deleted}");

    let dry = sync(&["--dry-run"]);
    assert_eq!(dry["carried"].as_array().unwrap().len(), 2, "{dry}");
    assert!(dir.join("proj/data.json").exists());

    let r = sync(&[]);
    assert_eq!(
        r["carried"],
        serde_json::json!([
            {"from": "proj/data.json", "to": "archive/proj/data.json"},
            {"from": "proj/img/x.png", "to": "archive/proj/img/x.png"}
        ])
    );
    assert!(dir.join("archive/proj/img/x.png").exists() && dir.join("archive/proj/sub/b.md").exists() && !dir.join("proj").exists());
    assert_eq!(r["left_behind"][0]["path"], "other", "{r}");
    assert!(r["left_behind"][0]["reason"].as_str().unwrap().contains("1 pdf"));
    assert!(dir.join("other/deck.pdf").exists() && !dir.join("other/c.md").exists());
    // Directories holding nothing are listed; one holding only a skipped folder is not.
    assert_eq!(r["empty_dirs"], serde_json::json!(["empty", "empty/deeper"]));

    let pruned = sync(&["--prune-empty-dirs"]);
    assert_eq!(pruned["removed_empty_dirs"], 2);
    assert!(!dir.join("empty").exists() && dir.join("keep/node_modules/m.js").exists());
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
    // Hidden folders are synced, except Obsidian's trash (and .git, .textdb, node_modules).
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    std::fs::create_dir_all(dir.join(".trash")).unwrap();
    std::fs::write(dir.join(".claude/rules.md"), "rules\n").unwrap();
    std::fs::write(dir.join(".trash/old.md"), "old\n").unwrap();
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
    assert_eq!(first["to_textdb"]["new"], serde_json::json!([".claude/rules.md", "a.md", "b.md", "c.md", "sub/d.md"]), "{first}");
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
    assert_eq!(quiet["unchanged"], 6, "{quiet}");
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
