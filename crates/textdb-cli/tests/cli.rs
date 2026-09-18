//! The CLI end to end against a SQLite store: import, browse, edit by line number against a
//! version read earlier, conflicts, history, hunks, the change log, and following changes
//! made by another process.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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

/// Does making a file read-only actually stop this process writing to it?
///
/// On Unix root ignores the permission bits, so a test that makes a file read-only to provoke a
/// write failure quietly tests nothing — and asserts the opposite of what happens. Containers
/// usually run as root, which is where anyone would be debugging. Asking the filesystem beats
/// asking who we are: it is the property the test actually depends on, and it needs no extra
/// dependency and no per-platform guess (on Windows the bit binds administrators too).
fn read_only_blocks_writes() -> bool {
    let Ok(dir) = tempfile::tempdir() else { return true };
    let probe = dir.path().join("probe");
    if std::fs::write(&probe, b"before").is_err() {
        return true;
    }
    let Ok(meta) = std::fs::metadata(&probe) else { return true };
    let mut perms = meta.permissions();
    perms.set_readonly(true);
    if std::fs::set_permissions(&probe, perms).is_err() {
        return true;
    }
    let blocked = std::fs::write(&probe, b"after").is_err();
    // Put the bit back so the temporary directory can remove the file on Windows.
    if let Ok(meta) = std::fs::metadata(&probe) {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(&probe, perms);
    }
    blocked
}

fn textdb(store: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_textdb"));
    // Asset bindings and caches in the test's own folder, never the user's.
    let config = store.parent().map(|p| p.join("config")).unwrap_or_else(|| std::env::temp_dir().join("textdb-cli-test-config"));
    cmd.env_remove("TEXTDB_STORE")
        .env_remove("TEXTDB_AUTHOR")
        .env_remove("TEXTDB_PATH_HISTORY")
        .env("TEXTDB_CONFIG_DIR", config)
        // Root discovery walks up from the working directory, so a synced directory anywhere
        // above the test would pair it with a store it knows nothing about. The ceiling stops
        // the walk where it starts, which is what a test wants: only what it set up itself.
        .env("TEXTDB_CEILING_DIRECTORIES", std::env::current_dir().unwrap_or_default())
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
    // `folders.parent` is `dir` now: one name for the parent on every surface.
    assert_eq!(sql(&["--format", "lines", "SELECT dir FROM folders WHERE path = '/p/old'"]).stdout, "/p\n");
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
    assert!(broken.contains("[deck](../docs/deck.pdf) -> /docs/deck.pdf (not-pulled)"), "{broken}");
    let pulled = ok(&mut t(&["--json", "assets", "pull", "--linked-from", "/notes"]), None).json();
    assert_eq!(pulled["pulled"].as_array().unwrap().len(), 1, "{pulled}");
    assert_eq!(std::fs::read(vault.join("docs/deck.pdf")).unwrap(), b"%PDF-1.4 one");
    assert_eq!(ok(&mut t(&["--json", "assets", "verify"]), None).json()["problems"], 0);

    // A file somebody else put in the asset store, which no pointer names: told of, and no problem
    // of textdb's to count -- whose bytes those are is not for textdb to decide. The assets' own
    // files are named by their pointers, so none of them is listed here.
    std::fs::create_dir_all(bucket.join("img")).unwrap();
    std::fs::write(bucket.join("img/nobodys.png"), b"\x89PNG theirs").unwrap();
    let listed = ok(&mut t(&["--json", "assets", "verify"]), None).json();
    assert_eq!(listed["problems"], 0, "{listed}");
    let unnamed: Vec<&str> = listed["unnamed"].as_array().unwrap().iter().filter_map(|u| u["at"].as_str()).collect();
    assert_eq!(unnamed, vec!["/img/nobodys.png"], "{listed}");
    // One asset, or one folder, asked about on its own never lists a store's files: what a store
    // holds that nothing names is a whole vault's question, and listing a drive is not free.
    let one = ok(&mut t(&["--json", "assets", "verify", "/img"]), None).json();
    assert_eq!(one["unnamed"].as_array().unwrap().len(), 0, "{one}");
    std::fs::remove_file(bucket.join("img/nobodys.png")).unwrap();

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
fn asset_pointers_never_reach_into_git_or_tell_of_other_files() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (vault, bucket, config) = (tmp.path().join("vault"), tmp.path().join("bucket"), tmp.path().join("config"));
    std::fs::create_dir_all(vault.join("img")).unwrap();
    std::fs::create_dir_all(vault.join(".git").join("hooks")).unwrap();
    std::fs::create_dir_all(&bucket).unwrap();
    std::fs::write(vault.join("img/evil.dat"), b"#!/bin/sh\necho pwned\n\0").unwrap();
    std::fs::write(vault.join(".env"), "SECRET=hunter2\n").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    ok(&mut t(&["assets", "push"]), None);
    let pointer = std::fs::read_to_string(vault.join("img/evil.dat.tdbasset")).unwrap();

    // Pointers anyone could write to the store: one inside .git, one naming a file that is no asset.
    ok(&mut t(&["write", "/.git/hooks/post-checkout.tdbasset"]), Some(&pointer));
    ok(&mut t(&["write", "/.env.tdbasset"]), Some(&pointer));
    let s = ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json();
    let find = |p: &str| s["assets"].as_array().unwrap().iter().find(|a| a["path"] == p).cloned().unwrap_or_else(|| panic!("{p} not in {s}"));
    let hook = find("/.git/hooks/post-checkout");
    assert_eq!(hook["state"], "invalid-path", "{hook}");
    let env = find("/.env");
    assert_eq!(env["state"], "conflict", "{env}");
    assert!(env.get("size").is_none() && env.get("file").is_none(), "nothing of the file is told: {env}");

    // Neither is pulled.
    run(&mut t(&["assets", "pull", "--dir", dir]), None);
    assert!(!vault.join(".git/hooks/post-checkout").exists());
    assert_eq!(std::fs::read_to_string(vault.join(".env")).unwrap(), "SECRET=hunter2\n");

    // Nor does a sync write a document from the store into .git (in any letter case).
    ok(&mut t(&["write", "/.git/hooks/notes.md"]), Some("#!/bin/sh\necho pwned\n"));
    ok(&mut t(&["write", "/sub/.GIT/config.md"]), Some("[core]\n"));
    let synced = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert!(!vault.join(".git/hooks/notes.md").exists() && !vault.join("sub/.GIT/config.md").exists(), "{}", synced.stdout);
    assert!(synced.stdout.contains("sync never writes into"), "{}", synced.stdout);

    // Nor into textdb's own folder, dependencies or trash, nor into .git by another name Windows
    // gives it (the store may refuse some of these names, which is as good).
    for rel in ["/.textdb/bin/evil.md", "/node_modules/evil.md", "/.trash/evil.md"] {
        ok(&mut t(&["write", rel]), Some("#!/bin/sh\necho pwned\n"));
    }
    for rel in ["/GIT~1/hooks/short.md", "/.git./hooks/dotted.md"] {
        run(&mut t(&["write", rel]), Some("#!/bin/sh\necho pwned\n"));
    }
    let synced = run(&mut t(&["--json", "sync", "/", dir]), None);
    for rel in [".textdb/bin/evil.md", "node_modules/evil.md", ".trash/evil.md", ".git/hooks/short.md", "GIT~1/hooks/short.md", ".git/hooks/dotted.md"] {
        assert!(!vault.join(rel).exists(), "{rel} was written: {}", synced.stdout);
    }

    // push --force never sends a file a pointer only names to the asset store.
    run(&mut t(&["assets", "push", "--force", "--dir", dir]), None);
    assert!(!bucket.join(".env").exists(), "the .env went to the asset store");

    // A submodule's .git file is not overwritten through its 8.3 short name.
    std::fs::create_dir_all(vault.join("sub")).unwrap();
    std::fs::write(vault.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    run(&mut t(&["write", "/sub/GIT~1"]), Some("gitdir: ../evil\n"));
    run(&mut t(&["sync", "/", dir]), None);
    assert_eq!(std::fs::read_to_string(vault.join("sub/.git")).unwrap(), "gitdir: ../.git/modules/sub\n");

    // Nothing is written through a link or junction already in the directory, in any letter case,
    // by sync or by pull.
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    if link_dir(&outside, &vault.join("link")) {
        ok(&mut t(&["write", "/link/hooks/exact.md"]), Some("x\n"));
        run(&mut t(&["write", "/LINK/hooks/case.md"]), Some("x\n"));
        ok(&mut t(&["write", "/link/pic.dat.tdbasset"]), Some(&pointer));
        run(&mut t(&["sync", "/", dir]), None);
        run(&mut t(&["assets", "pull", "--dir", dir]), None);
        let written: Vec<_> = std::fs::read_dir(&outside).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(written.is_empty(), "written through the link: {written:?}");
    }

    // A .gitattributes arriving from textdb does not make other files assets in the same sync, nor
    // in the next one before the rules are accepted.
    std::fs::write(vault.join("secret.txt"), "TOKEN=1\n").unwrap();
    ok(&mut t(&["write", "/.gitattributes"]), Some("secret.txt textdb=asset\n"));
    run(&mut t(&["sync", "/", dir, "--push"]), None);
    assert!(!bucket.join("secret.txt").exists(), "pushed in the sync that brought the rule");
    run(&mut t(&["sync", "/", dir, "--push"]), None);
    assert!(!bucket.join("secret.txt").exists(), "pushed before the rules were accepted");

    // Nor under another spelling of its name: other letter case (which Windows loads all the same),
    // or the 8.3 short name of the .gitattributes already there.
    let rules = std::fs::read_to_string(vault.join(".gitattributes")).unwrap();
    std::fs::write(vault.join("secret2.txt"), "TOKEN=2\n").unwrap();
    run(&mut t(&["write", "/sub2/.GitAttributes"]), Some("secret2.txt textdb=asset\n"));
    run(&mut t(&["write", "/GITATT~1"]), Some("secret2.txt textdb=asset\n"));
    for _ in 0..2 {
        run(&mut t(&["sync", "/", dir, "--push"]), None);
        assert!(!bucket.join("secret2.txt").exists(), "pushed under a rule with another name");
    }
    assert_eq!(std::fs::read_to_string(vault.join(".gitattributes")).unwrap(), rules, "written through its short name");
}

#[test]
fn store_pointers_never_move_or_trash_rules_files_nor_sync_write_through_short_folder_names() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (vault, bucket, config) = (tmp.path().join("vault"), tmp.path().join("bucket"), tmp.path().join("config"));
    for d in ["attachments", "config", "project files"] {
        std::fs::create_dir_all(vault.join(d)).unwrap();
    }
    std::fs::create_dir_all(&bucket).unwrap();
    let png: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01];
    std::fs::write(vault.join(".gitattributes"), "private.pdf textdb=ignore").unwrap();
    std::fs::write(vault.join("private.pdf"), "%PDF-1.4 private").unwrap();
    std::fs::write(vault.join("attachments/.gitattributes"), "* binary").unwrap();
    std::fs::write(vault.join("attachments/pic.png"), png).unwrap();
    // An asset with the very bytes of the root rules, so a pointer can name them.
    std::fs::write(vault.join("attachments/copy.dat"), std::fs::read(vault.join(".gitattributes")).unwrap()).unwrap();
    std::fs::write(vault.join("config/.env"), "SECRET=1").unwrap();
    std::fs::write(vault.join("config/readme.md"), "config notes").unwrap();
    std::fs::write(vault.join("project files/settings.json"), "LOCAL").unwrap();
    std::fs::write(vault.join("project files/note.md"), "local note").unwrap();
    std::fs::write(vault.join("my~secrets file.json"), "LOCAL").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    ok(&mut t(&["assets", "push"]), None);
    run(&mut t(&["sync", "/", dir, "--push"]), None);
    assert!(bucket.join("attachments/copy.dat").exists() && !bucket.join("private.pdf").exists());
    let with_id = |n: u32| {
        let pointer = std::fs::read_to_string(vault.join("attachments/copy.dat.tdbasset")).unwrap();
        pointer.lines().map(|l| if l.starts_with("id: ") { format!("id: 01a0a37c-6564-709a-9f90-00000000000{n}") } else { l.to_string() }).collect::<Vec<_>>().join("\n") + "\n"
    };

    // A pointer deleted in the store does not take the rules file it names to the trash.
    ok(&mut t(&["write", "/.gitattributes.tdbasset"]), Some(&with_id(1)));
    run(&mut t(&["sync", "/", dir, "--push"]), None);
    ok(&mut t(&["rm", "/.gitattributes.tdbasset"]), None);
    let synced = run(&mut t(&["sync", "/", dir, "--push"]), None);
    assert!(vault.join(".gitattributes").exists(), "the rules went to the trash: {}", synced.stdout);
    assert!(!bucket.join("private.pdf").exists(), "pushed without its rule: {}", synced.stdout);

    // A pointer moved in the store does not carry the rules file it names to another folder.
    ok(&mut t(&["write", "/attachments/.gitattributes.tdbasset"]), Some(&with_id(2)));
    run(&mut t(&["sync", "/", dir, "--push"]), None);
    ok(&mut t(&["mv", "/attachments/.gitattributes.tdbasset", "/config/.gitattributes.tdbasset"]), None);
    let synced = run(&mut t(&["sync", "/", dir, "--push"]), None);
    assert!(vault.join("attachments/.gitattributes").exists() && !vault.join("config/.gitattributes").exists(), "carried: {}", synced.stdout);
    assert!(!bucket.join("config/.env").exists(), "pushed under carried rules: {}", synced.stdout);

    // Nothing is written through the 8.3 short name of a folder, or of a name that has a `~` itself.
    for rel in ["/PROJEC~1/settings.json", "/PROJEC~1/note.md", "/MY~SEC~1.JSO"] {
        run(&mut t(&["write", rel]), Some("FROM STORE"));
    }
    let synced = run(&mut t(&["sync", "/", dir]), None);
    for (rel, was) in [("project files/settings.json", "LOCAL"), ("project files/note.md", "local note"), ("my~secrets file.json", "LOCAL")] {
        assert_eq!(std::fs::read_to_string(vault.join(rel)).unwrap(), was, "{rel} written through a short name: {}", synced.stdout);
    }
}

#[test]
fn textdbignore_leaves_obsidian_code_out_both_ways_and_stays_the_directorys_own() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join(".obsidian/plugins/local")).unwrap();
    std::fs::write(vault.join(".obsidian/plugins/local/main.md"), "local plugin").unwrap();
    std::fs::write(vault.join("a.md"), "a").unwrap();
    std::fs::write(vault.join(".textdbignore"), "*.tmp\n").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.args(args);
        c
    };

    // The first sync adds the default lines to the rules the directory has, and leaves out what they match.
    ok(&mut t(&["sync", "/", dir]), None);
    let rules = std::fs::read_to_string(vault.join(".textdbignore")).unwrap();
    assert!(rules.starts_with("*.tmp\n") && rules.contains("**/.obsidian/plugins\n"), "{rules}");
    assert_eq!(run(&mut t(&["stat", "/.obsidian/plugins/local/main.md"]), None).status, 5, "a plugin was taken in");
    assert_eq!(run(&mut t(&["stat", "/.textdbignore"]), None).status, 5, "the rules were taken in");

    // Nor is a plugin written from the store, at any depth, nor a file a `!` line names inside a
    // folder left out, nor anything under the rules' name.
    let rules = format!("{rules}!*.css\n");
    std::fs::write(vault.join(".textdbignore"), &rules).unwrap();
    ok(&mut t(&["sync", "/", dir]), None);
    for rel in ["/.obsidian/plugins/evil/main.js", "/sub/.obsidian/themes/t/theme.css", "/.obsidian/snippets/evil.css", "/.textdbignore/readme.md"] {
        ok(&mut t(&["write", rel]), Some("from the store\n"));
    }
    let synced = run(&mut t(&["sync", "/", dir]), None);
    for rel in [".obsidian/plugins/evil", "sub/.obsidian/themes", ".obsidian/snippets/evil.css"] {
        assert!(!vault.join(rel).exists(), "{rel} written: {}", synced.stdout);
    }
    assert_eq!(std::fs::read_to_string(vault.join(".textdbignore")).unwrap(), rules, "the store changed the rules");
    assert!(synced.stdout.contains("left out by .textdbignore"), "{}", synced.stdout);

    // Loosening the rules stops the sync until accepted. The directory's own rules decide: without
    // those lines plugins sync, and the defaults are not added again.
    std::fs::write(vault.join(".textdbignore"), "").unwrap();
    let stopped = run(&mut t(&["sync", "/", dir]), None);
    assert_eq!(stopped.status, 6, "{}", stopped.stdout);
    assert!(!vault.join(".obsidian/plugins/evil").exists(), "{}", stopped.stdout);
    let synced = run(&mut t(&["sync", "/", dir, "--accept-rules"]), None);
    assert!(vault.join(".obsidian/plugins/evil/main.js").exists() && vault.join("sub/.obsidian/themes/t/theme.css").exists(), "{}", synced.stdout);
    assert_eq!(run(&mut t(&["stat", "/.obsidian/plugins/local/main.md"]), None).status, 0, "{}", synced.stdout);
    assert_eq!(std::fs::read_to_string(vault.join(".textdbignore")).unwrap(), "");
    std::fs::remove_file(vault.join(".textdbignore")).unwrap();
    run(&mut t(&["sync", "/", dir]), None);
    assert!(!vault.join(".textdbignore").exists(), "the defaults came back after the file was deleted");
}

#[test]
fn defaults_are_added_unless_the_rules_say_otherwise_and_binary_notes_are_never_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    let lines: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    std::fs::write(vault.join("note.txt"), &lines).unwrap();
    std::fs::write(vault.join(".textdbignore"), "# .obsidian/themes are shared\n.obsidian/plugins/*/data.json\n").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.args(args);
        c
    };

    // A comment or a pattern for some files in a folder is no say on the folder: its default goes in.
    ok(&mut t(&["sync", "/", dir]), None);
    let rules = std::fs::read_to_string(vault.join(".textdbignore")).unwrap();
    assert!(rules.contains("**/.obsidian/plugins\n") && rules.contains("**/.obsidian/themes\n"), "{rules}");

    // A note that turned binary on disk is not merged with a change in the store.
    let mut bytes = lines.clone().into_bytes();
    bytes[5] = 0;
    std::fs::write(vault.join("note.txt"), &bytes).unwrap();
    run(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["write", "/note.txt"]), Some(&lines.replace("line 18\n", "line 18 (store)\n")));
    let synced = run(&mut t(&["sync", "/", dir]), None);
    assert!(synced.stdout.contains("a binary file"), "{}", synced.stdout);
    assert_eq!(std::fs::read(vault.join("note.txt")).unwrap(), bytes, "the binary on disk changed: {}", synced.stdout);
    assert_eq!(ok(&mut t(&["--json", "stat", "/note.txt"]), None).json()["version"], 2, "{}", synced.stdout);

    // Rules that are not UTF-8 stop the sync.
    std::fs::write(vault.join(".textdbignore"), [0xff, 0xfe, b'*', 0]).unwrap();
    let stopped = run(&mut t(&["sync", "/", dir]), None);
    assert_ne!(stopped.status, 0, "{}", stopped.stdout);
    assert!(format!("{}{}", stopped.stdout, stopped.stderr).contains("UTF-8"), "{}", stopped.stderr);
}

#[test]
fn loosening_textdbignore_stops_moves_and_changes_of_what_it_left_out_even_with_a_bom() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("secret")).unwrap();
    std::fs::write(vault.join("secret/s.md"), "s").unwrap();
    std::fs::write(vault.join("a.md"), "a").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.args(args);
        c
    };
    ok(&mut t(&["sync", "/", dir]), None);
    let bom = "\u{feff}";
    std::fs::write(vault.join(".textdbignore"), format!("{bom}secret/\n{SEEDED_RULES}")).unwrap();
    run(&mut t(&["sync", "/", dir]), None);

    // The line goes and the file moves on disk: the store's file is not moved.
    std::fs::write(vault.join(".textdbignore"), format!("{bom}# nothing\n{SEEDED_RULES}")).unwrap();
    std::fs::create_dir_all(vault.join("notes")).unwrap();
    std::fs::rename(vault.join("secret/s.md"), vault.join("notes/s.md")).unwrap();
    let stopped = run(&mut t(&["sync", "/", dir]), None);
    assert_eq!(stopped.status, 6, "{}", stopped.stdout);
    assert_eq!(run(&mut t(&["stat", "/secret/s.md"]), None).status, 0, "{}", stopped.stdout);

    // Nor is a change on disk taken in.
    std::fs::rename(vault.join("notes/s.md"), vault.join("secret/s.md")).unwrap();
    std::fs::write(vault.join("secret/s.md"), "changed on disk").unwrap();
    let stopped = run(&mut t(&["sync", "/", dir]), None);
    assert_eq!(stopped.status, 6, "{}", stopped.stdout);
    assert_eq!(ok(&mut t(&["--json", "stat", "/secret/s.md"]), None).json()["nbytes"], 1, "{}", stopped.stdout);
}

#[test]
fn loosening_textdbignore_stops_a_folder_move_carrying_what_it_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("proj")).unwrap();
    std::fs::write(vault.join("proj/a.md"), "a").unwrap();
    std::fs::write(vault.join("proj/secret.json"), "{}").unwrap();
    std::fs::write(vault.join(".textdbignore"), format!("proj/secret.json\n{SEEDED_RULES}")).unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.args(args);
        c
    };
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["mv", "/proj", "/proj2"]), None);
    std::fs::write(vault.join(".textdbignore"), SEEDED_RULES).unwrap();
    let stopped = run(&mut t(&["sync", "/", dir]), None);
    assert_eq!(stopped.status, 6, "{}", stopped.stdout);
    assert!(vault.join("proj/secret.json").exists() && !vault.join("proj2/secret.json").exists(), "{}", stopped.stdout);
}

#[test]
fn binary_files_are_not_taken_in_and_sync_says_what_to_do() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("a.md"), "a").unwrap();
    std::fs::write(vault.join("blob.md"), [b'x', 0, b'y']).unwrap();
    std::fs::write(vault.join("data.txt"), "text").unwrap();
    let dir = vault.to_str().unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.args(args);
        c
    };
    let synced = run(&mut t(&["sync", "/", dir]), None);
    assert_eq!(synced.status, 0, "{}", synced.stdout);
    assert_eq!(run(&mut t(&["stat", "/a.md"]), None).status, 0);
    assert_eq!(run(&mut t(&["stat", "/blob.md"]), None).status, 5, "a binary file was taken in");
    assert!(synced.stdout.contains("blob.md") && synced.stdout.contains(".textdbignore"), "{}", synced.stdout);

    // A text file that turns binary is not taken in either: the store keeps its text.
    std::fs::write(vault.join("data.txt"), [b't', 0]).unwrap();
    let synced = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert!(synced.stdout.contains("data.txt") && synced.stdout.contains("a binary file"), "{}", synced.stdout);
    let stat = ok(&mut t(&["--json", "stat", "/data.txt"]), None).json();
    assert_eq!(stat["nbytes"], 4, "{stat}");
}

#[test]
fn a_first_sync_into_a_new_folder_writes_no_obsidian_code_and_unreadable_rules_stop_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.args(args);
        c
    };
    for (rel, text) in [
        ("/notes/a.md", "a"),
        ("/notes/.obsidian/app.json", "{}"),
        ("/notes/.obsidian/plugins/evil/main.js", "run()"),
        ("/notes/vault/.obsidian/themes/t/theme.css", "x"),
    ] {
        ok(&mut t(&["write", rel]), Some(text));
    }

    // A directory that does not exist yet gets the default rules before anything is written.
    let fresh = tmp.path().join("fresh");
    let synced = run(&mut t(&["sync", "/notes", fresh.to_str().unwrap()]), None);
    assert!(fresh.join("a.md").exists() && fresh.join(".obsidian/app.json").exists(), "{}", synced.stdout);
    assert!(!fresh.join(".obsidian/plugins").exists() && !fresh.join("vault/.obsidian/themes").exists(), "{}", synced.stdout);
    assert!(fresh.join(".textdbignore").is_file());

    // Rules that are there but cannot be read stop the sync before anything is written.
    let other = tmp.path().join("other");
    std::fs::create_dir_all(other.join(".textdbignore")).unwrap();
    let stopped = run(&mut t(&["sync", "/notes", other.to_str().unwrap()]), None);
    assert_ne!(stopped.status, 0, "{}", stopped.stdout);
    assert!(!other.join("a.md").exists() && !other.join(".obsidian").exists(), "{}", stopped.stdout);
}

/// Make `link` a link to the folder `target`: a junction on Windows (which needs no privilege), a
/// symbolic link elsewhere.
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().is_ok_and(|o| o.status.success())
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

/// rclone as the driver runs it: without flags from `RCLONE_*` variables (its configuration still
/// comes through), which could send a test's commands somewhere the driver does not go.
fn rclone_command(exe: &std::path::Path) -> Command {
    let mut c = Command::new(exe);
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        let upper = key.to_ascii_uppercase();
        if upper.starts_with("RCLONE_") && !upper.starts_with("RCLONE_CONFIG") && upper != "RCLONE_PASSWORD_COMMAND" {
            c.env_remove(key);
        }
    }
    c
}

/// rclone for tests: `TEXTDB_RCLONE` only, as CI sets it, never one found on the PATH. These tests
/// rewrite and rename files quickly, which security software on a person's own computer may take
/// for ransomware. Without it the test is skipped, unless `TEXTDB_REQUIRE_RCLONE` is set.
fn test_rclone() -> Option<std::path::PathBuf> {
    let exe = std::env::var_os("TEXTDB_RCLONE").filter(|e| !e.is_empty()).map(std::path::PathBuf::from);
    let runs = exe.as_ref().is_some_and(|exe| Command::new(exe).arg("version").output().is_ok_and(|o| o.status.success()));
    if !runs && std::env::var_os("TEXTDB_REQUIRE_RCLONE").is_some() {
        panic!("TEXTDB_REQUIRE_RCLONE is set, but TEXTDB_RCLONE does not name an rclone that runs");
    }
    exe.filter(|_| runs)
}

/// A Google Drive folder called `textdb-test` to test against (`TEXTDB_TEST_GDRIVE`, such as
/// `gdrive:textdb-test`), and rclone; the test is skipped without one.
fn test_gdrive() -> Option<(std::path::PathBuf, String)> {
    let base = std::env::var("TEXTDB_TEST_GDRIVE").ok().filter(|b| !b.is_empty())?;
    let base = base.trim_end_matches('/').to_string();
    assert_eq!(base.rsplit(['/', ':']).next(), Some("textdb-test"), "TEXTDB_TEST_GDRIVE must name a folder called textdb-test, not {base}");
    let rclone = test_rclone().expect("TEXTDB_TEST_GDRIVE is set, but rclone does not run");
    let remote = base.split_once(':').map_or("", |(r, _)| r).to_string();
    let listed = rclone_command(&rclone).args(["listremotes", "--long"]).output().unwrap();
    let on_drive = String::from_utf8_lossy(&listed.stdout).lines().any(|l| l.split_once(':').is_some_and(|(n, t)| n.trim() == remote && t.split_whitespace().next() == Some("drive")));
    assert!(on_drive, "TEXTDB_TEST_GDRIVE must be on a Google Drive remote, not {base}");
    Some((rclone, base))
}

#[test]
fn assets_on_google_drive_are_pulled_by_file_id_and_never_from_outside_the_store() {
    let Some((rclone, base)) = test_gdrive() else { return };
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    let run_dir = format!("{base}/cli-{}-{nanos}", std::process::id());
    let root = format!("{run_dir}/store");
    let rc = |args: &[&str]| {
        let out = rclone_command(&rclone).args(args).output().unwrap();
        assert!(out.status.success(), "rclone {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    struct Purge(std::path::PathBuf, String);
    impl Drop for Purge {
        fn drop(&mut self) {
            let _ = rclone_command(&self.0).args(["purge", "--drive-use-trash=false", &self.1]).output();
        }
    }
    rc(&["mkdir", &root]);
    let _purge = Purge(rclone.clone(), run_dir.clone());
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (vault, config) = (tmp.path().join("vault"), tmp.path().join("config"));
    std::fs::create_dir_all(vault.join("img")).unwrap();
    std::fs::write(vault.join("notes.md"), "![[a.png]]\n").unwrap();
    let bytes = [137u8, 80, 78, 71, 0, 1, 2];
    std::fs::write(vault.join("img/a.png"), bytes).unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).env("TEXTDB_RCLONE", &rclone).args(args);
        c
    };
    let dir = vault.to_str().unwrap();
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "stores", "--add", "drive", "--driver", "rclone", "--root", &root]), None);
    ok(&mut t(&["assets", "push", "--dir", dir]), None);
    let pointer = std::fs::read_to_string(vault.join("img/a.png.tdbasset")).unwrap();
    let id = pointer.lines().find_map(|l| l.strip_prefix("item: ")).unwrap().to_string();
    assert!(!id.starts_with('/'), "the item is the Drive file id: {pointer}");

    // What the drive holds is the asset's own state while the file here is still the bytes its
    // pointer names: moved there it is `moved-in-store` and says where it went, trashed there it is
    // `trashed-in-store`. Neither is the vault's to settle, and a pull still fetches either.
    let asset_at = |path: &str| {
        ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json()["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["path"] == path)
            .cloned()
            .unwrap_or_else(|| panic!("{path} is not in `assets status`"))
    };
    // Renamed in the drive and then trashed there: pulled all the same, by its id.
    rc(&["moveto", &format!("{root}/img/a.png"), &format!("{root}/elsewhere/renamed.png")]);
    let moved_in_store = asset_at("/img/a.png");
    assert_eq!(moved_in_store["state"], "moved-in-store", "{moved_in_store}");
    assert!(
        moved_in_store["in_store"].as_str().is_some_and(|at| at.ends_with("elsewhere/renamed.png")),
        "the asset does not say where its file went in the drive: {moved_in_store}"
    );
    rc(&["deletefile", &format!("{root}/elsewhere/renamed.png")]);
    let trashed_in_store = asset_at("/img/a.png");
    assert_eq!(trashed_in_store["state"], "trashed-in-store", "{trashed_in_store}");
    std::fs::remove_file(vault.join("img/a.png")).unwrap();
    ok(&mut t(&["assets", "pull", "--dir", dir]), None);
    assert_eq!(std::fs::read(vault.join("img/a.png")).unwrap(), bytes);

    // A pointer naming a file outside the store is never pulled, whatever that file is.
    rc(&["copyto", vault.join("notes.md").to_str().unwrap(), &format!("{run_dir}/outside/notes.md")]);
    let outside: serde_json::Value = serde_json::from_str(&rc(&["lsjson", "--stat", &format!("{run_dir}/outside/notes.md")])).unwrap();
    let outside_id = outside["ID"].as_str().unwrap();
    let planted = pointer
        .lines()
        .map(|l| match l {
            l if l.starts_with("item: ") => format!("item: {outside_id}"),
            l if l.starts_with("id: ") => "id: 01a0a37c-6564-709a-9f90-00000000000c".to_string(),
            l => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    ok(&mut t(&["write", "/img/c.png.tdbasset"]), Some(&planted));
    run(&mut t(&["sync", "/", dir]), None);
    let pulled = run(&mut t(&["assets", "pull", "--dir", dir]), None);
    assert!(format!("{}{}", pulled.stdout, pulled.stderr).contains("names no file of the asset store"), "{} {}", pulled.stdout, pulled.stderr);
    assert!(!vault.join("img/c.png").exists());
    // Nothing about this vault is wrong once the bytes its pointer names are here -- but the store
    // holds no file of that item, so that is what the asset says of itself, and neither a pull nor
    // a push guesses at which file was meant. Taken away again, so what follows sees it as it was.
    std::fs::write(vault.join("img/c.png"), bytes).unwrap();
    let invalid_item = asset_at("/img/c.png");
    assert_eq!(invalid_item["state"], "invalid-item", "{invalid_item}");
    std::fs::remove_file(vault.join("img/c.png")).unwrap();

    // A pointer deleted in textdb sends its file to Drive's trash -- but only once every pointer of
    // the store can be read, since what bytes are for is not guessed at from the pointers that
    // happened to parse. Two assets: one deleted while a pointer cannot be read, one after.
    for (name, last) in [("d", 3u8), ("e", 4), ("f", 5)] {
        std::fs::write(vault.join(format!("img/{name}.png")), [137u8, 80, 78, 71, 0, last]).unwrap();
    }
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "push", "--dir", dir]), None);
    let item_of = |name: &str| {
        std::fs::read_to_string(vault.join(format!("img/{name}.png.tdbasset")))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("item: ").map(str::to_string))
            .unwrap()
    };
    let (id_d, id_e, id_f) = (item_of("d"), item_of("e"), item_of("f"));
    let live = || rc(&["lsjson", "-R", "--files-only", &root]);
    let there = live();
    assert!([&id_d, &id_e, &id_f].iter().all(|id| there.contains(id.as_str())), "all three files are in the drive after the push: {there}");

    // A pointer textdb cannot read counts as naming those bytes: nothing goes to Drive's trash, and
    // the sync says which pointer it could not read rather than leaving it to be guessed at.
    ok(&mut t(&["write", "/img/bad.png.tdbasset"]), Some("not a pointer at all\n"));
    run(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["rm", "/img/d.png"]), None);
    let unreadable = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert!(live().contains(&id_d), "a file went to Drive's trash while a pointer of the store could not be read");
    let said = format!("{}{}", unreadable.stdout, unreadable.stderr);
    assert!(said.contains("cannot be read") && said.contains("bad.png"), "the sync did not say which pointer it could not read: {said}");

    // Bytes someone replaced in the drive are theirs: a deleted pointer does not take them away,
    // whatever its own bytes were, and the sync says so. The file keeps its id through the
    // replacement, so only the bytes tell the two apart.
    ok(&mut t(&["rm", "/img/bad.png.tdbasset"]), None);
    run(&mut t(&["sync", "/", dir]), None);
    let theirs = vault.join("theirs.png");
    std::fs::write(&theirs, [137u8, 80, 78, 71, 9, 9]).unwrap();
    let at_e = rc(&["lsjson", "-R", "--files-only", &root]);
    let path_of_e = serde_json::from_str::<Vec<serde_json::Value>>(&at_e)
        .unwrap()
        .into_iter()
        .find(|l| l["ID"].as_str() == Some(id_e.as_str()))
        .map(|l| l["Path"].as_str().unwrap().to_string())
        .unwrap();
    rc(&["copyto", "--ignore-times", theirs.to_str().unwrap(), &format!("{root}/{path_of_e}")]);
    std::fs::remove_file(&theirs).unwrap();
    // The file here is still the one the pointer names; the drive's is somebody else's now. The
    // asset says so, and a pull would take those bytes as its new version rather than refuse them.
    let changed_in_store = asset_at("/img/e.png");
    assert_eq!(changed_in_store["state"], "changed-in-store", "{changed_in_store}");
    ok(&mut t(&["rm", "/img/e.png"]), None);
    let kept = run(&mut t(&["--json", "sync", "/", dir]), None);
    assert!(live().contains(&id_e), "bytes replaced in the drive were sent to Drive's trash by a deleted pointer");
    let said = format!("{}{}", kept.stdout, kept.stderr);
    assert!(said.contains("other than the ones its pointer named"), "the sync did not say the bytes in the drive are not the pointer's: {said}");

    // A pointer whose file in the drive still holds the bytes it named: that one does go to Drive's
    // trash, where a pull by id would still find it for the thirty days Drive keeps it.
    ok(&mut t(&["rm", "/img/f.png"]), None);
    ok(&mut t(&["sync", "/", dir]), None);
    assert!(!live().contains(&id_f), "the file is still live in the drive: {}", live());
    let gone = rc(&["lsjson", "-R", "--files-only", "--drive-trashed-only", &root]);
    assert!(gone.contains(&id_f), "the file is not in Drive's trash: {gone}");

    // An asset moved in textdb leaves its file in the drive where it was: nothing moves a person's
    // drive about on its own. The asset is `moved-here`, and the sync says so with the way out.
    std::fs::write(vault.join("img/g.png"), [137u8, 80, 78, 71, 0, 6]).unwrap();
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "push", "--dir", dir]), None);
    let id_g = item_of("g");
    let where_of = |id: &str| {
        serde_json::from_str::<Vec<serde_json::Value>>(&rc(&["lsjson", "-R", "--files-only", &root]))
            .unwrap()
            .into_iter()
            .find(|l| l["ID"].as_str() == Some(id))
            .map(|l| l["Path"].as_str().unwrap().to_string())
    };
    assert_eq!(where_of(&id_g).as_deref(), Some("img/g.png"));
    ok(&mut t(&["mv", "/img/g.png", "/pics/g.png"]), None);
    let after_mv = ok(&mut t(&["--json", "sync", "/", dir]), None).json();
    assert_eq!(where_of(&id_g).as_deref(), Some("img/g.png"), "`mv` moved the file in the drive");
    let notes = format!("{}", after_mv["assets"]["notes"]);
    assert!(notes.contains("/pics/g.png") && notes.contains("relocate"), "the sync did not say the asset moved and how to settle it: {notes}");
    let status = ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json();
    let moved_here = status["assets"].as_array().unwrap().iter().find(|a| a["path"] == "/pics/g.png").unwrap().clone();
    assert_eq!(moved_here["state"], "moved-here", "{moved_here}");
    assert_eq!(moved_here["in_store"], "/img/g.png", "{moved_here}");

    // `relocate` moves it in the drive to the asset's own path, and Drive keeps the file's id
    // through the move, so the pointer needs no rewriting and links in the drive still point at it.
    ok(&mut t(&["assets", "relocate", "--dir", dir, "--dry-run"]), None);
    assert_eq!(where_of(&id_g).as_deref(), Some("img/g.png"), "a dry run moved it");
    ok(&mut t(&["assets", "relocate", "--dir", dir]), None);
    assert_eq!(where_of(&id_g).as_deref(), Some("pics/g.png"), "the file did not move to the asset's path");
    // Read where the pointer is now: the sync moved it with the asset, and Drive keeps the file's
    // id through the move, so the pointer still names what it named before.
    let item_at = |rel: &str| {
        std::fs::read_to_string(vault.join(rel))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("item: ").map(str::to_string))
            .unwrap()
    };
    assert_eq!(item_at("pics/g.png.tdbasset"), id_g, "the move gave the file a new id");
    let settled = ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json();
    let g = settled["assets"].as_array().unwrap().iter().find(|a| a["path"] == "/pics/g.png").unwrap().clone();
    assert_eq!(g["state"], "ok", "{g}");
    // Of this asset: others in this test were left broken on purpose (one in Drive's trash, one
    // naming a file outside the store), and verify counts those as the problems they are.
    assert_eq!(run(&mut t(&["assets", "verify", "/pics/g.png", "--dir", dir]), None).status, 0);
}

#[test]
fn assets_through_an_rclone_store() {
    let Some(rclone) = test_rclone() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (vault, other, bucket, config) = (tmp.path().join("vault"), tmp.path().join("other"), tmp.path().join("remote bucket"), tmp.path().join("config"));
    std::fs::create_dir_all(vault.join("img")).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::create_dir_all(&bucket).unwrap();
    std::fs::write(vault.join("notes.md"), "![[arch.png]]\n").unwrap();
    std::fs::write(vault.join("img/arch.png"), [137u8, 80, 78, 71, 0, 1]).unwrap();
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).env("TEXTDB_RCLONE", &rclone).args(args);
        c
    };
    let (dir, other_dir) = (vault.to_str().unwrap(), other.to_str().unwrap());
    ok(&mut t(&["sync", "/", dir]), None);

    // The root is shared with everyone using the store: only a remote of their own rclone
    // configuration, never options (some run programs) or a flag.
    for bad in [":sftp,host=x,ssh='echo pwned':x", "remote,ssh=x:textdb", "-x:textdb"] {
        let refused = run(&mut t(&["assets", "stores", "--add", "evil", "--driver", "rclone", &format!("--root={bad}")]), None);
        assert_eq!(refused.status, 6, "{bad}: {}", refused.stderr);
    }

    // A remote folder that is not there is reported; bound on this computer to where rclone
    // reaches the store, it is used.
    ok(&mut t(&["assets", "stores", "--add", "team", "--driver", "rclone", "--root", "nowhere-remote:textdb"]), None);
    let stores = ok(&mut t(&["--json", "assets", "stores"]), None).json();
    assert_eq!(stores[0]["reachable"], false, "{stores}");
    assert_eq!(run(&mut t(&["assets", "push"]), None).status, 1);
    let remote = bucket.to_str().unwrap().replace('\\', "/");
    ok(&mut t(&["assets", "stores", "--bind", &format!("team={remote}")]), None);
    let stores = ok(&mut t(&["--json", "assets", "stores"]), None).json();
    assert_eq!(stores[0]["reachable"], true, "{stores}");

    let pushed = ok(&mut t(&["--json", "assets", "push"]), None).json();
    assert_eq!(pushed["pushed"].as_array().unwrap().len(), 1, "{pushed}");
    assert_eq!(std::fs::read(bucket.join("img/arch.png")).unwrap(), [137u8, 80, 78, 71, 0, 1]);

    // Changed here: replaced there, the old bytes in the remote's trash; rclone flags set in the
    // environment do not change that.
    std::fs::write(vault.join("img/arch.png"), [137u8, 80, 78, 71, 0, 2]).unwrap();
    ok(&mut t(&["assets", "push"]).env("RCLONE_IGNORE_EXISTING", "true").env("RCLONE_DRY_RUN", "true"), None);
    assert_eq!(std::fs::read(bucket.join("img/arch.png")).unwrap(), [137u8, 80, 78, 71, 0, 2]);
    let trashed: Vec<_> = std::fs::read_dir(bucket.join(".textdb-trash")).unwrap().flatten().map(|e| e.path().join("img/arch.png")).filter(|p| p.is_file()).collect();
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    assert_eq!(std::fs::read(&trashed[0]).unwrap(), [137u8, 80, 78, 71, 0, 1]);

    // Pulled into another directory, and verified; bytes changed in the remote directly are caught.
    ok(&mut t(&["sync", "/", other_dir]), None);
    ok(&mut t(&["assets", "pull", "--dir", other_dir]), None);
    assert_eq!(std::fs::read(other.join("img/arch.png")).unwrap(), [137u8, 80, 78, 71, 0, 2]);
    let verified = run(&mut t(&["assets", "verify", "--dir", other_dir]), None);
    assert_eq!(verified.status, 0, "{}{}", verified.stdout, verified.stderr);
    std::fs::write(bucket.join("img/arch.png"), [0u8; 6]).unwrap();
    assert_eq!(run(&mut t(&["assets", "verify", "--dir", other_dir]), None).status, 1);
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
    if read_only_blocks_writes() {
        set_readonly(true);
        let failed = run(&mut t(&["assets", "push", "--dir", d]), None);
        set_readonly(false);
        assert_eq!(failed.status, 1, "{}", failed.stdout);
        ok(&mut t(&["sync", "/", d]), None);
        assert_eq!(std::fs::read_to_string(&on_disk).unwrap(), ok(&mut t(&["cat", "/img/a.png.tdbasset"]), None).stdout);
        assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", d]), None).json()["counts"], serde_json::json!({ "ok": 2 }));
    } else {
        // Running as root (a container, usually), where the read-only bit does not bind: the
        // push would succeed and this would assert the opposite of what happens. Push it the
        // ordinary way instead, so the rest of the test carries on from the same state.
        eprintln!("skipping the unwritable-pointer case: read-only files are writable by this user");
        ok(&mut t(&["assets", "push", "--dir", d]), None);
        assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", d]), None).json()["counts"], serde_json::json!({ "ok": 2 }));
    }

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
    // One directory paired with a second folder: the config names one, so this says so on purpose.
    ok(&mut t(&["sync", "--force", "/w2", dw]), None);
    std::fs::write(w.join("z.png"), b"\x89PNG z").unwrap();
    ok(&mut t(&["assets", "push", "--dir", dw]), None);
    let s = ok(&mut t(&["--json", "assets", "status", "--dir", dw]), None).json();
    assert_eq!((s["prefix"].as_str(), &s["counts"]), (Some("/w2"), &serde_json::json!({ "ok": 1 })), "{s}");
    let other = run(&mut t(&["--json", "sync", "--dry-run", "--force", "/w1", dw]), None).json();
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
fn sync_pairs_assets_with_their_real_files() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (v1, v2, bucket, config) = (tmp.path().join("v1"), tmp.path().join("v2"), tmp.path().join("bucket"), tmp.path().join("config"));
    for d in [v1.join("img"), v2.clone(), bucket.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let (d1, d2) = (v1.to_str().unwrap(), v2.to_str().unwrap());
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    std::fs::write(v1.join("notes.md"), "![[a.png]] ![[b.png]]\n").unwrap();
    for (name, bytes) in [("a", &b"\x89PNG A"[..]), ("b", b"\x89PNG B"), ("c", b"\x89PNG C")] {
        std::fs::write(v1.join(format!("img/{name}.png")), bytes).unwrap();
    }

    // Assets are never taken in as documents, whatever --ext takes.
    let all = ok(&mut t(&["--json", "sync", "--dry-run", "--ext", "*", "/", d1]), None).json();
    assert!(!all["to_textdb"]["new"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().ends_with(".png")), "{all}");

    // --push pushes new assets after the documents; --pull fetches what the notes link to.
    let s1 = ok(&mut t(&["--json", "sync", "--push", "/", d1]), None).json();
    assert_eq!(s1["assets"]["pushed"], serde_json::json!(["/img/a.png", "/img/b.png", "/img/c.png"]), "{s1}");
    let s2 = ok(&mut t(&["--json", "sync", "--pull", "/", d2]), None).json();
    assert_eq!(s2["assets"]["pulled"], serde_json::json!(["/img/a.png", "/img/b.png"]), "{s2}");
    assert_eq!(s2["assets"]["counts"], serde_json::json!({ "not-pulled": 1, "ok": 2 }), "{s2}");

    // Moved in the store (by the asset's own path): the file moves with its pointer.
    ok(&mut t(&["mv", "/img/a.png", "/pics/a.png"]), None);
    let moved = ok(&mut t(&["--json", "sync", "/", d2]), None).json();
    assert!(v2.join("pics/a.png").exists() && !v2.join("img/a.png").exists(), "{moved}");

    // Deleted in the store: the file goes to the directory's trash.
    ok(&mut t(&["rm", "/img/b.png"]), None);
    let deleted = ok(&mut t(&["--json", "sync", "/", d2]), None).json();
    assert_eq!(deleted["assets"]["trashed"], serde_json::json!(["img/b.png"]), "{deleted}");
    assert!(!v2.join("img/b.png").exists());
    let trash = deleted["assets"]["trash"].as_str().unwrap();
    assert_eq!(std::fs::read(v2.join(trash).join("img/b.png")).unwrap(), b"\x89PNG B");
    // Its copy in the asset store stays where it is: a local store keeps no trash of the
    // provider's own, and those bytes are there for whatever else may name them.
    assert_eq!(std::fs::read(bucket.join("img/b.png")).unwrap(), b"\x89PNG B", "a deleted pointer took the local store's copy with it");

    // Renamed on disk: the pointer follows its file.
    ok(&mut t(&["sync", "/", d1]), None);
    assert!(v1.join("pics/a.png").exists() && !v1.join("img/b.png").exists());
    std::fs::rename(v1.join("img/c.png"), v1.join("img/renamed.png")).unwrap();
    let renamed = ok(&mut t(&["--json", "sync", "/", d1]), None).json();
    assert_eq!(renamed["assets"]["renamed"], serde_json::json!([{ "from": "img/c.png", "to": "img/renamed.png" }]), "{renamed}");
    assert_eq!(run(&mut t(&["stat", "/img/renamed.png.tdbasset"]), None).status, 0);
    assert_eq!(run(&mut t(&["stat", "/img/c.png.tdbasset"]), None).status, 5);
    assert!(v1.join("img/renamed.png.tdbasset").exists() && !v1.join("img/c.png.tdbasset").exists());

    // Changed on both sides: the store's bytes come in, this directory's are kept next to them.
    std::fs::write(v1.join("pics/a.png"), b"\x89PNG A from v1").unwrap();
    ok(&mut t(&["sync", "--push", "/", d1]), None);
    std::fs::write(v2.join("pics/a.png"), b"\x89PNG A from v2").unwrap();
    let both = ok(&mut t(&["--json", "sync", "/", d2]), None).json();
    let copies = both["assets"]["conflict_copies"].as_array().unwrap();
    assert_eq!(copies.len(), 1, "{both}");
    let copy = copies[0]["to"].as_str().unwrap();
    assert!(copy.starts_with("pics/a (conflict ") && copy.ends_with(").png"), "{copy}");
    assert_eq!(std::fs::read(v2.join(copy)).unwrap(), b"\x89PNG A from v2");
    assert_eq!(std::fs::read(v2.join("pics/a.png")).unwrap(), b"\x89PNG A from v1");
    // The copy is never pushed on its own.
    let again = ok(&mut t(&["--json", "sync", "--push", "/", d2]), None).json();
    assert_eq!((&again["assets"]["pushed"], &again["assets"]["counts"]["conflict-copy"]), (&serde_json::json!([]), &serde_json::json!(1)), "{again}");

    // The store's asset_sync setting decides when the command line does not.
    ok(&mut t(&["setting", "asset_sync", "both"]), None);
    std::fs::write(v1.join("img/new.png"), b"\x89PNG N").unwrap();
    let auto = ok(&mut t(&["--json", "sync", "/", d1]), None).json();
    assert_eq!((auto["assets"]["mode"].as_str(), &auto["assets"]["pushed"]), (Some("both"), &serde_json::json!(["/img/new.png"])), "{auto}");
}

#[test]
fn sync_pairing_leaves_stray_copies_and_letter_case_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let dirs: Vec<std::path::PathBuf> = ["v1", "v2", "v3", "bucket", "config"].iter().map(|d| tmp.path().join(d)).collect();
    let (v1, v2, v3, bucket, config) = (&dirs[0], &dirs[1], &dirs[2], &dirs[3], &dirs[4]);
    for d in [v1.join("img"), v2.clone(), v3.join("other"), bucket.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", config).args(args);
        c
    };
    let (d1, d2, d3) = (v1.to_str().unwrap(), v2.to_str().unwrap(), v3.to_str().unwrap());
    let exists = |path: &str| run(&mut t(&["stat", path]), None).status == 0;
    let names = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir.join("img")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_lowercase()).collect()
    };
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    std::fs::write(v1.join("notes.md"), "![[a.png]]\n").unwrap();
    std::fs::write(v1.join("img/a.png"), b"\x89PNG A").unwrap();
    std::fs::write(v1.join("img/b.png"), b"\x89PNG B").unwrap();
    ok(&mut t(&["sync", "--push", "/", d1]), None);
    ok(&mut t(&["sync", "--pull", "/", d2]), None);

    // A copy of an asset's bytes in a directory that never had the asset is not a rename of it.
    ok(&mut t(&["sync", "/", d3]), None);
    std::fs::write(v3.join("other/logo.png"), b"\x89PNG B").unwrap();
    ok(&mut t(&["sync", "/", d3]), None);
    assert!(exists("/img/b.png.tdbasset") && !exists("/other/logo.png.tdbasset"));

    // A name changed only in letter case, here or in the store, keeps the pointer and every file.
    std::fs::rename(v1.join("img/a.png"), v1.join("img/A.png")).unwrap();
    for _ in 0..3 {
        ok(&mut t(&["sync", "/", d1]), None);
    }
    assert!(exists("/img/a.png.tdbasset") || exists("/img/A.png.tdbasset"));
    let (now, other) = if exists("/img/a.png.tdbasset") { ("/img/a.png", "/img/A.png") } else { ("/img/A.png", "/img/a.png") };
    ok(&mut t(&["mv", now, other]), None);
    for _ in 0..2 {
        ok(&mut t(&["sync", "/", d2]), None);
    }
    let in_v2 = names(v2);
    assert!(in_v2.contains(&"a.png".to_string()) && in_v2.contains(&"a.png.tdbasset".to_string()), "{in_v2:?}");
    assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", d2]), None).json()["counts"]["ok"], 1);

    // A pointer moved to a name without its suffix stays a pointer; a folder is no asset's name.
    ok(&mut t(&["mv", "/img/b.png.tdbasset", "/img/b2.png"]), None);
    assert!(exists("/img/b2.png.tdbasset") && !exists("/img/b2.png"));
    assert_eq!(run(&mut t(&["mv", "/img/b2.png", "/img"]), None).status, 6);
    assert_eq!(run(&mut t(&["mv", "/img/b2.png", "/pics/"]), None).status, 6);

    // The asset settings show their own defaults.
    assert_eq!(ok(&mut t(&["--json", "setting", "asset_sync"]), None).json()["asset_sync"]["effective"], "off");
    assert_eq!(ok(&mut t(&["--json", "setting", "asset_pull"]), None).json()["asset_pull"]["effective"], "linked");

    // Changed .gitattributes files stop automatic pushes, sync after sync, until accepted; what they
    // make an asset is never taken in as a document.
    ok(&mut t(&["setting", "asset_sync", "push"]), None);
    std::fs::write(v1.join(".gitattributes"), "*.txt textdb=asset\n").unwrap();
    std::fs::write(v1.join("draft.txt"), "a text the rules make an asset\n").unwrap();
    for _ in 0..2 {
        let s = ok(&mut t(&["--json", "sync", "/", d1]), None).json();
        assert_eq!(s["assets"]["pushed"], serde_json::json!([]), "{s}");
        assert!(!s["to_textdb"]["new"].as_array().unwrap().iter().any(|p| p == "draft.txt"), "{s}");
    }
    let accepted = ok(&mut t(&["--json", "sync", "--accept-rules", "/", d1]), None).json();
    assert_eq!(accepted["assets"]["pushed"], serde_json::json!(["/draft.txt"]), "{accepted}");
}

#[test]
fn sync_takes_recreated_files_and_leaves_old_copies_and_moved_names_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let (v1, v2, bucket, config) = (tmp.path().join("v1"), tmp.path().join("v2"), tmp.path().join("bucket"), tmp.path().join("config"));
    for d in [v1.join("img"), v1.join("notes"), v2.clone(), bucket.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let t = |args: &[&str]| {
        let mut c = textdb(&store);
        c.env("TEXTDB_CONFIG_DIR", &config).args(args);
        c
    };
    let (d1, d2) = (v1.to_str().unwrap(), v2.to_str().unwrap());
    let exists = |path: &str| run(&mut t(&["stat", path]), None).status == 0;
    ok(&mut t(&["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    std::fs::write(v1.join("notes/n.md"), "old\n").unwrap();
    std::fs::write(v1.join("img/a.png"), b"\x89PNG A").unwrap();
    ok(&mut t(&["sync", "--push", "/", d1]), None);
    ok(&mut t(&["sync", "/", d2]), None);
    ok(&mut t(&["assets", "pull", "--dir", d2]), None);

    // A document deleted and made again at the same path (version 1 again) reaches the other directory.
    ok(&mut t(&["rm", "/notes/n.md"]), None);
    ok(&mut t(&["write", "/notes/n.md"]), Some("NEW\n"));
    ok(&mut t(&["sync", "/", d2]), None);
    assert_eq!(std::fs::read_to_string(v2.join("notes/n.md")).unwrap(), "NEW\n");

    // A copy that was there at the last sync is not the new name of an asset whose file went away.
    std::fs::create_dir_all(v2.join("backup")).unwrap();
    std::fs::write(v2.join("backup/old.png"), b"\x89PNG A").unwrap();
    ok(&mut t(&["sync", "/", d2]), None);
    std::fs::remove_file(v2.join("img/a.png")).unwrap();
    ok(&mut t(&["sync", "/", d2]), None);
    assert!(exists("/img/a.png.tdbasset") && !exists("/backup/old.png.tdbasset"));
    // Nor is a copy that turns up after the asset's file had been gone for a sync.
    ok(&mut t(&["sync", "/", d2]), None);
    std::fs::create_dir_all(v2.join("downloads")).unwrap();
    std::fs::write(v2.join("downloads/logo.png"), b"\x89PNG A").unwrap();
    ok(&mut t(&["sync", "/", d2]), None);
    assert!(exists("/img/a.png.tdbasset") && !exists("/downloads/logo.png.tdbasset"));

    // An asset moved on disk with its pointer: a new file at the old name is new, not an orphan.
    std::fs::create_dir_all(v1.join("pics")).unwrap();
    std::fs::rename(v1.join("img/a.png"), v1.join("pics/a.png")).unwrap();
    std::fs::rename(v1.join("img/a.png.tdbasset"), v1.join("pics/a.png.tdbasset")).unwrap();
    ok(&mut t(&["sync", "/", d1]), None);
    assert!(exists("/pics/a.png.tdbasset") && !exists("/img/a.png.tdbasset"));
    std::fs::write(v1.join("img/a.png"), b"\x89PNG another").unwrap();
    let s = ok(&mut t(&["--json", "assets", "status", "--dir", d1]), None).json();
    assert_eq!(s["counts"], serde_json::json!({ "new": 1, "ok": 1 }), "{s}");

    // An asset cannot take a document's name, in any letter case, nor a document an asset's, even
    // with another folder of the same name in another case (`/Pics` lists before `/pics`).
    ok(&mut t(&["write", "/Pics/z.md"]), Some("Z\n"));
    assert_eq!(run(&mut t(&["mv", "/pics/a.png", "/notes/n.md"]), None).status, 6);
    assert_eq!(run(&mut t(&["mv", "/pics/a.png", "/NOTES/N.md"]), None).status, 6);
    assert_eq!(run(&mut t(&["mv", "/notes/n.md", "/PICS/A.png"]), None).status, 6);
    // A rename in letter case only, beyond ASCII too, is not in its own way.
    ok(&mut t(&["mv", "/pics/a.png", "/pics/Ä.png"]), None);
    ok(&mut t(&["mv", "/pics/Ä.png", "/pics/ä.png"]), None);
    assert!(exists("/pics/ä.png.tdbasset") && !exists("/pics/Ä.png.tdbasset"));
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
    std::fs::write(v.join(".textdbignore"), SEEDED_RULES).unwrap();
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

    // A vault in a subfolder of the repository.
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("vault/img")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("vault/img/z.png"), b"\x89PNG z").unwrap();
    std::fs::write(repo.join("README.md"), "r\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "start"]);
    let dv = repo.join("vault");
    ok(&mut t(&["sync", "/sub", dv.to_str().unwrap()]), None);
    let sub = ok(&mut t(&["--json", "assets", "migrate-from-git", "--dir", dv.to_str().unwrap()]), None).json();
    assert_eq!(sub["migrated"], serde_json::json!(["/sub/img/z.png"]), "{sub}");
    let tracked = git(&repo, &["ls-files"]);
    assert!(tracked.lines().any(|l| l == "vault/img/z.png.tdbasset") && !tracked.lines().any(|l| l == "vault/img/z.png"), "{tracked}");
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

    // A bare sync keeps the rules the pairing recorded, so it is not a widening.
    let same = sync(&[]);
    assert_eq!(same.status, 0, "{}", same.stderr);
    assert!(same.json().get("rules").is_none(), "{}", same.stdout);

    // Wider extensions, asked for: the sync stops and lists what they would take in.
    let wider = sync(&["--ext", "md,txt"]);
    assert_eq!(wider.status, 6, "{}", wider.stdout);
    let w = wider.json();
    assert_eq!((w["stopped_by_rules"].as_bool(), &w["rules"]["newly_included"]), (Some(true), &serde_json::json!(["notes.txt"])));
    assert_eq!(run(textdb(&store).args(["stat", "/notes.txt"]), None).status, 5);
    let accepted = sync(&["--ext", "md,txt", "--accept-rules"]);
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

/// A `.textdbignore` that already has the lines sync puts in one, as a checkout that synced before
/// keeps it: sync leaves it as it is.
const SEEDED_RULES: &str = "**/.obsidian/plugins\n**/.obsidian/snippets\n**/.obsidian/themes\n";

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
    std::fs::write(docs.join(".textdbignore"), SEEDED_RULES).unwrap();
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
    // A second store for the same directory: the pairing names one, so this says so on purpose.
    let boot = ok(textdb(&older).args(["--json", "sync", "--force", "--base", "HEAD~1", "/docs"]).arg(&docs), None).json();
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
    // `--versions-only` filters the rows and keeps the row type: it used to drop the `type`
    // tag that tells a version from a path event, so one command emitted two JSON shapes.
    let versions = ok(textdb(&store).args(["--json", "history", "/b/y.md", "--versions-only"]), None).json();
    assert_eq!(versions.as_array().unwrap().len(), 1);
    assert_eq!(versions[0]["type"], "version", "{versions}");

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

// ---------------------------------------------------------------------------
// The rclone driver against a provider that misbehaves
//
// `assets_through_an_rclone_store` above runs against real rclone on its local backend, which
// proves the command line and the JSON are right. It cannot prove anything about the paths that
// only run against a real provider, because the local backend never misbehaves: it always keeps
// a SHA-256, never rewrites what it stores, and renames atomically. Those paths carry the most
// risk in the driver and had no test at all. `textdb-fake-rclone` stands in for rclone and can
// be told to behave as the providers documentably do.
// ---------------------------------------------------------------------------

/// A store, a vault synced with it, and an rclone store served by the stand-in.
///
/// Returns a command builder whose environment points `TEXTDB_RCLONE` at the stand-in, plus the
/// vault and the directory the "remote" keeps its bytes in.
fn fake_rclone_vault(tmp: &Path, name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let (store, vault, remote, config) =
        (tmp.join("kb.db"), tmp.join("vault"), tmp.join("remote"), tmp.join("config"));
    std::fs::create_dir_all(vault.join("img")).unwrap();
    // The root the stand-in serves; the driver addresses it as `fake:NAME`.
    std::fs::create_dir_all(remote.join(name)).unwrap();
    (store, vault, remote, config)
}

/// `textdb` with the stand-in wired in as rclone.
fn with_fake_rclone(store: &Path, remote: &Path, config: &Path, args: &[&str]) -> Command {
    let mut c = textdb(store);
    c.env("TEXTDB_CONFIG_DIR", config)
        .env("TEXTDB_RCLONE", env!("CARGO_BIN_EXE_textdb-fake-rclone"))
        .env("TEXTDB_FAKE_RCLONE_ROOT", remote)
        .args(args);
    c
}

/// The stand-in is faithful enough to push, pull and verify through: if this fails, nothing the
/// other tests in this group claim about the driver means anything.
#[test]
fn fake_rclone_round_trips_a_push_and_a_pull() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, vault, remote, config) = fake_rclone_vault(tmp.path(), "textdb");
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(vault.join("notes.md"), "![[arch.png]]\n").unwrap();
    std::fs::write(vault.join("img/arch.png"), b"\x89PNG first").unwrap();
    let t = |args: &[&str]| with_fake_rclone(&store, &remote, &config, args);
    let (dir, other_dir) = (vault.to_str().unwrap(), other.to_str().unwrap());

    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "stores", "--add", "drive", "--driver", "rclone", "--root", "fake:textdb"]), None);
    let stores = ok(&mut t(&["--json", "assets", "stores"]), None).json();
    assert_eq!(stores[0]["reachable"], true, "{stores}");

    let pushed = ok(&mut t(&["--json", "assets", "push", "--dir", dir]), None).json();
    assert_eq!(pushed["pushed"].as_array().unwrap().len(), 1, "{pushed}");
    assert_eq!(std::fs::read(remote.join("textdb/img/arch.png")).unwrap(), b"\x89PNG first");
    assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json()["counts"], serde_json::json!({ "ok": 1 }));
    assert_eq!(ok(&mut t(&["--json", "assets", "verify", "--dir", dir]), None).json()["problems"], 0);

    // And out again into a second directory.
    ok(&mut t(&["sync", "/", other_dir]), None);
    ok(&mut t(&["assets", "pull", "--dir", other_dir]), None);
    assert_eq!(std::fs::read(other.join("img/arch.png")).unwrap(), b"\x89PNG first");
}

/// OneDrive and SharePoint hash with QuickXorHash, so `--hash-type SHA256` gives nothing back and
/// the driver has to read the bytes to hash them. Real rclone's local backend always answers with
/// a SHA-256, so this fallback has never run under test — and it is what every push verification
/// and every `verify` against those providers would go through.
#[test]
fn an_asset_store_that_keeps_no_sha256_is_hashed_by_reading_it_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, vault, remote, config) = fake_rclone_vault(tmp.path(), "textdb");
    std::fs::write(vault.join("notes.md"), "![[a.png]]\n").unwrap();
    std::fs::write(vault.join("img/a.png"), b"\x89PNG no provider hash").unwrap();
    let t = |args: &[&str]| {
        let mut c = with_fake_rclone(&store, &remote, &config, args);
        c.env("TEXTDB_FAKE_RCLONE_NO_SHA256", "1");
        c
    };
    let dir = vault.to_str().unwrap();
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "stores", "--add", "sharepoint", "--driver", "rclone", "--root", "fake:textdb"]), None);

    ok(&mut t(&["assets", "push", "--dir", dir]), None);
    assert_eq!(std::fs::read(remote.join("textdb/img/a.png")).unwrap(), b"\x89PNG no provider hash");
    assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json()["counts"], serde_json::json!({ "ok": 1 }));
    // `verify` hashes both sides, so it exercises the read-back path on purpose.
    assert_eq!(ok(&mut t(&["--json", "assets", "verify", "--dir", dir]), None).json()["problems"], 0);

    // And it still catches bytes changed in the store directly, which is the point of hashing.
    std::fs::write(remote.join("textdb/img/a.png"), b"tampered").unwrap();
    assert_eq!(run(&mut t(&["assets", "verify", "--dir", dir]), None).status, 1);
}

/// SharePoint "silently modifies uploaded files, mainly Office files (.docx, .xlsx, etc.), causing
/// file size and hash checks to fail" (rclone's own OneDrive documentation). The bytes that land
/// are then not the bytes the pointer names, so the push must fail and commit no pointer — never
/// publish a pointer whose sha256 nothing in the store matches.
///
/// This is the failure every Office-file push against real SharePoint would hit today, and the
/// reason stage 3's second part wants a provider version tag rather than a hash comparison.
#[test]
fn an_asset_store_that_rewrites_uploads_fails_the_push_and_publishes_no_pointer() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, vault, remote, config) = fake_rclone_vault(tmp.path(), "textdb");
    std::fs::write(vault.join("notes.md"), "![[report.docx]]\n").unwrap();
    std::fs::write(vault.join("img/report.docx"), b"PK\x03\x04 office bytes").unwrap();
    let t = |args: &[&str]| {
        let mut c = with_fake_rclone(&store, &remote, &config, args);
        c.env("TEXTDB_FAKE_RCLONE_REWRITE", "1");
        c
    };
    let dir = vault.to_str().unwrap();
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "stores", "--add", "sharepoint", "--driver", "rclone", "--root", "fake:textdb"]), None);

    let failed = run(&mut t(&["assets", "push", "--dir", dir]), None);
    assert_eq!(failed.status, 1, "{}{}", failed.stdout, failed.stderr);
    assert!(failed.stdout.contains("report.docx"), "{}", failed.stdout);

    // No pointer was committed, so the asset is still waiting to be published rather than
    // recorded as stored under a hash the store cannot produce.
    assert_eq!(run(&mut t(&["stat", "/img/report.docx.tdbasset"]), None).status, 5);
    let counts = ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json()["counts"].clone();
    assert_eq!(counts, serde_json::json!({ "new": 1 }), "{counts}");

    // "One asset failing does not stop the others": a file the provider leaves alone goes up in
    // the same run, and the run still exits 1 for the one that did not.
    std::fs::write(vault.join("img/plain.png"), b"\x89PNG untouched").unwrap();
    ok(&mut t(&["sync", "/", dir]), None);
    let both = run(&mut t(&["--json", "assets", "push", "--dir", dir]), None);
    assert_eq!(both.status, 1, "{}{}", both.stdout, both.stderr);
    let report: Value = serde_json::from_str(both.stdout.lines().next().unwrap()).unwrap();
    let pushed: Vec<&str> = report["pushed"].as_array().unwrap().iter().map(|p| p["path"].as_str().unwrap()).collect();
    assert_eq!(pushed, ["/img/plain.png"], "{}", both.stdout);
    assert_eq!(report["failed"].as_array().unwrap().len(), 1, "{}", both.stdout);
    assert!(report["failed"][0].as_str().unwrap().contains("report.docx"), "{}", both.stdout);
    assert_eq!(std::fs::read(remote.join("textdb/img/plain.png")).unwrap(), b"\x89PNG untouched");

    // The rewritten bytes never become the asset: no pointer names them, under its own path or
    // the `beside` name a push falls back to when other bytes hold the path.
    assert_eq!(run(&mut t(&["stat", "/img/report.docx.tdbasset"]), None).status, 5);
}

/// rclone clears the destination before a server-side move, so an asset is briefly missing from
/// its own path while it is replaced. When the move then fails, what was there has to come back:
/// the one path in the driver where a provider hiccup could lose an asset outright. Real rclone on
/// a local backend renames atomically and never leaves that gap.
#[test]
fn a_move_that_clears_the_destination_and_fails_puts_the_old_bytes_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, vault, remote, config) = fake_rclone_vault(tmp.path(), "textdb");
    std::fs::write(vault.join("notes.md"), "![[a.png]]\n").unwrap();
    std::fs::write(vault.join("img/a.png"), b"\x89PNG published").unwrap();
    let plain = |args: &[&str]| with_fake_rclone(&store, &remote, &config, args);
    let dir = vault.to_str().unwrap();
    ok(&mut plain(&["sync", "/", dir]), None);
    ok(&mut plain(&["assets", "stores", "--add", "drive", "--driver", "rclone", "--root", "fake:textdb"]), None);
    ok(&mut plain(&["assets", "push", "--dir", dir]), None);
    let stored = remote.join("textdb/img/a.png");
    assert_eq!(std::fs::read(&stored).unwrap(), b"\x89PNG published");

    // A new version of the asset, pushed while every server-side move fails after clearing the
    // destination.
    std::fs::write(vault.join("img/a.png"), b"\x89PNG the next version").unwrap();
    let mut gap = with_fake_rclone(&store, &remote, &config, &["assets", "push", "--dir", dir]);
    let failed = run(gap.env("TEXTDB_FAKE_RCLONE_MOVE_GAP", "1"), None);
    assert_eq!(failed.status, 1, "{}{}", failed.stdout, failed.stderr);

    // The published bytes are back where the asset belongs: not missing, and not the half-written
    // new version.
    assert_eq!(std::fs::read(&stored).unwrap(), b"\x89PNG published", "the asset must not be left missing or replaced");
    // And the store still describes what is actually there.
    assert_eq!(ok(&mut plain(&["--json", "assets", "verify", "--dir", dir]), None).json()["problems"], 0);

    // Once the provider behaves, the same push goes through.
    ok(&mut plain(&["assets", "push", "--dir", dir]), None);
    assert_eq!(std::fs::read(&stored).unwrap(), b"\x89PNG the next version");
}

/// The lock protocol rests on the provider listing what was just written ("of two pushes that
/// wrote at once, the one that lists later sees the other's file"). Google Drive promises no such
/// thing. A push must still finish when its own lock file takes several listings to appear, rather
/// than wedging or giving up.
#[test]
fn a_push_finishes_when_the_provider_lists_its_lock_file_late() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, vault, remote, config) = fake_rclone_vault(tmp.path(), "textdb");
    std::fs::write(vault.join("notes.md"), "![[a.png]]\n").unwrap();
    std::fs::write(vault.join("img/a.png"), b"\x89PNG late listing").unwrap();
    let t = |args: &[&str]| {
        let mut c = with_fake_rclone(&store, &remote, &config, args);
        // Every lock file stays unlisted for its first few listings.
        c.env("TEXTDB_FAKE_RCLONE_LIST_LAG", "6");
        c
    };
    let dir = vault.to_str().unwrap();
    ok(&mut t(&["sync", "/", dir]), None);
    ok(&mut t(&["assets", "stores", "--add", "drive", "--driver", "rclone", "--root", "fake:textdb"]), None);

    let started = std::time::Instant::now();
    ok(&mut t(&["assets", "push", "--dir", dir]), None);
    // It has to converge, not wait out the ten-minute lock budget.
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "the push took {:?}", started.elapsed());
    assert_eq!(std::fs::read(remote.join("textdb/img/a.png")).unwrap(), b"\x89PNG late listing");
    assert_eq!(ok(&mut t(&["--json", "assets", "status", "--dir", dir]), None).json()["counts"], serde_json::json!({ "ok": 1 }));

    // No lock file is left behind for the next push to wait on.
    let locks = remote.join("textdb/.textdb-trash/locks");
    let left: Vec<_> = std::fs::read_dir(&locks).into_iter().flatten().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "locks left behind: {left:?}");
}

/// The front-matter property index: what a vault uses, what each property holds, and the
/// query language over both.
///
/// The counts matter as much as the paths: a list has one row per element, so a note tagged
/// three ways must still count once, and `meta keys` reporting three would quietly mislead
/// every UI that shows it.
#[test]
fn properties_are_indexed_and_queryable_by_name_and_value() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    let note = |title: &str, status: &str, tags: &str, priority: u32, extra: &str| {
        format!(
            "---\ntitle: {title}\nstatus: {status}\ntags: [{tags}]\npriority: {priority}\nproject:\n  name: atlas\n  phase: pilot\n{extra}---\n\n# {title}\n\nbody\n"
        )
    };
    ok(textdb(&store).args(["write", "/a.md"]), Some(&note("A", "draft", "cvm, telco", 5, "budget: 120\n")));
    ok(textdb(&store).args(["write", "/b.md"]), Some(&note("B", "review", "telco", 2, "")));
    ok(textdb(&store).args(["write", "/c.md"]), Some(&note("C", "draft", "cvm", 1, "")));
    // No front matter at all: it must not appear in any property answer.
    ok(textdb(&store).args(["write", "/plain.md"]), Some("# Plain\n\nno front matter\n"));

    let keys = ok(textdb(&store).args(["--json", "meta", "keys"]), None).json();
    let by_key: std::collections::HashMap<String, serde_json::Value> = keys
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["key"].as_str().unwrap().to_string(), r.clone()))
        .collect();
    // Three documents carry `tags`, not five rows' worth.
    assert_eq!(by_key["tags"]["docs"], 3, "a document with several tags counts once");
    assert_eq!(by_key["status"]["values"], 2, "draft and review");
    assert_eq!(by_key["priority"]["kind"], "number", "so a UI knows > means something here");
    assert_eq!(by_key["status"]["kind"], "text");
    assert_eq!(by_key["budget"]["docs"], 1, "a property only one note has is still listed");
    assert!(by_key.contains_key("project.name"), "nested keys are dotted: {:?}", by_key.keys());

    // The prefix is the autosuggest call.
    let pro = ok(textdb(&store).args(["--json", "meta", "keys", "pro"]), None).json();
    let names: Vec<&str> = pro.as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["project.name", "project.phase"]);

    let values = ok(textdb(&store).args(["--json", "meta", "values", "status"]), None).json();
    let vs: Vec<(&str, i64)> = values
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["value"].as_str().unwrap(), r["docs"].as_i64().unwrap()))
        .collect();
    assert_eq!(vs, vec![("draft", 2), ("review", 1)], "most-used first");

    let find = |q: &str| -> Vec<String> {
        let rows = ok(textdb(&store).args(["--json", "meta", "find", q]), None).json();
        rows.as_array().unwrap().iter().map(|r| r["path"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(find("status:draft"), vec!["/a.md", "/c.md"]);
    // A list contains a value: indexed one row per element, so this is an ordinary equality.
    assert_eq!(find("tags:telco"), vec!["/a.md", "/b.md"]);
    assert_eq!(find("status:draft tags:telco"), vec!["/a.md"], "a space means AND");
    assert_eq!(find("status:draft OR status:review").len(), 3);
    assert_eq!(find("-status:draft"), vec!["/b.md"]);
    assert_eq!(find("NOT status:draft"), vec!["/b.md"], "either spelling of NOT");
    assert_eq!(find("(status:draft OR status:review) has:budget"), vec!["/a.md"]);
    assert_eq!(find("project.name:atlas").len(), 3, "nested keys resolve");
    // Numbers compare numerically: lexically "5" would sort below "2" here.
    assert_eq!(find("priority:>3"), vec!["/a.md"]);
    assert_eq!(find("priority:<=2").len(), 2);
    assert_eq!(find("tags:c*"), vec!["/a.md", "/c.md"], "starts with");
    assert_eq!(find("title:~B"), vec!["/b.md"], "contains, ignoring case");
    // `!=` means "has it, but not as that" — /plain.md has no status and is not an answer.
    assert_eq!(find("status:!=draft"), vec!["/b.md"]);
    // The two partition the documents that have the property, exactly.
    assert_eq!(find("status:draft").len() + find("status:!=draft").len(), 3);
    // An empty query is every document that has front matter, so not /plain.md.
    assert_eq!(find("").len(), 3, "a document with no front matter is in no property answer");

    // Editing front matter moves the document between answers.
    ok(textdb(&store).args(["meta", "set", "/c.md", "status", "published"]), None);
    assert_eq!(find("status:draft"), vec!["/a.md"]);
    assert_eq!(find("status:published"), vec!["/c.md"]);
    // Removing the property removes the rows with it.
    ok(textdb(&store).args(["meta", "unset", "/c.md", "status"]), None);
    assert_eq!(find("has:status").len(), 2);
    // And deleting the document takes it out of every answer.
    ok(textdb(&store).args(["rm", "/b.md"]), None);
    assert_eq!(find("tags:telco"), vec!["/a.md"]);

    // A malformed query is an invalid edit (exit 6), named with the offset it went wrong at.
    let bad = run(textdb(&store).args(["meta", "find", "status:draft AND"]), None);
    assert_eq!(bad.status, 6, "{}", bad.stderr);
    assert!(bad.stderr.contains("ends early"), "{}", bad.stderr);
}

/// `meta find --show` prints property columns beside the path, and the `properties` view
/// exposes the same rows to SQL.
#[test]
fn property_columns_and_the_sql_view() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("kb.db");
    ok(
        textdb(&store).args(["write", "/a.md"]),
        Some("---\nstatus: draft\ntags: [cvm, telco]\npriority: 4\n---\n\n# A\n"),
    );
    let shown = ok(textdb(&store).args(["meta", "find", "status:draft", "--show", "status,tags,missing"]), None).stdout;
    assert!(shown.contains("status=draft"), "{shown}");
    // A list joins with commas rather than printing as JSON, and a property the document does
    // not have is a dash rather than a blank that reads as an empty value.
    assert!(shown.contains("tags=cvm,telco"), "{shown}");
    assert!(shown.contains("missing=-"), "{shown}");

    let rows = ok(
        textdb(&store).args(["--json", "sql", "SELECT key, value, ord FROM properties WHERE key = 'tags' ORDER BY ord"]),
        None,
    )
    .json();
    let vals: Vec<(&str, i64)> = rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["value"].as_str().unwrap(), r["ord"].as_i64().unwrap()))
        .collect();
    assert_eq!(vals, vec![("cvm", 0), ("telco", 1)], "a list keeps its order in `ord`");
}

/// The canonical listing record on SQLite: the same twenty-four keys, in the same order,
/// from `ls`, `stat` and `tree`, and identical to what Postgres returns (`cli_pg.rs`).
#[test]
fn every_listing_surface_returns_the_same_entry() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kb.db");
    ok(
        textdb(&db).args(["write", "/g/api/index.md"]),
        Some("---\ntitle: API Guide\nstatus: draft\n---\n# API Guide\n\nSee [limits](limits.md) and [gone](missing.md).\n\n## Errors\ntext\n"),
    );
    ok(textdb(&db).args(["write", "/g/api/limits.md"]), Some("# Limits\n\nrate limit is 100.\n"));

    const KEYS: [&str; 24] = [
        "path", "name", "kind", "version", "nbytes", "nlines", "updated_at", "updated_by", "id", "dir", "depth", "ext",
        "title", "nwords", "nsections", "nprops", "nlinks", "nlinks_broken", "versions", "created_at", "files",
        "folders", "nauthors", "authors",
    ];
    let keys_of = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };

    let stat = ok(textdb(&db).args(["--json", "stat", "/g/api/index.md"]), None).json();
    assert_eq!(keys_of(&stat), KEYS, "stat");
    let ls = ok(textdb(&db).args(["--json", "ls", "/g/api"]), None).json();
    assert_eq!(keys_of(&ls[0]), KEYS, "ls");
    let tree = ok(textdb(&db).args(["--json", "tree", "/g"]), None).json();
    assert_eq!(keys_of(&tree[0]), KEYS, "tree");
    // The same file through three commands is the same record, byte for byte.
    let from_ls = ls.as_array().unwrap().iter().find(|e| e["path"] == "/g/api/index.md").unwrap().clone();
    assert_eq!(from_ls, stat);

    assert_eq!(stat["title"], "API Guide");
    assert_eq!((stat["nsections"].as_i64(), stat["nprops"].as_i64()), (Some(2), Some(2)));
    assert_eq!((stat["nlinks"].as_i64(), stat["nlinks_broken"].as_i64()), (Some(2), Some(1)));
    assert_eq!((stat["ext"].as_str(), stat["dir"].as_str()), (Some("md"), Some("/g/api")));

    // A folder has totals and no version; both used to be null or meaningless (`v0`).
    let folder = ok(textdb(&db).args(["--json", "stat", "/g/api"]), None).json();
    assert!(folder["version"].is_null());
    assert_eq!(folder["nsections"], 3);
    assert_eq!(folder["nlinks_broken"], 1);
    assert!(folder["nbytes"].as_i64().unwrap() > 0);

    // `ls FILE` lists that one file rather than nothing, and `-1` marks folders.
    let one = ok(textdb(&db).args(["--json", "ls", "/g/api/index.md"]), None).json();
    assert_eq!(one.as_array().unwrap().len(), 1);
    let paths = ok(textdb(&db).args(["ls", "-1", "/g"]), None).stdout;
    assert!(paths.contains("/g/api/\n"), "a folder keeps its marker in -1: {paths:?}");
}

/// Creating a link's target fixes the count without a commit to the file holding the link.
#[test]
fn broken_link_counts_follow_the_target_not_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kb.db");
    ok(textdb(&db).args(["write", "/a.md"]), Some("# A\n\n[later](later.md)\n"));
    let before = ok(textdb(&db).args(["--json", "stat", "/a.md"]), None).json();
    assert_eq!(before["nlinks_broken"], 1);

    // Nothing writes to /a.md here; the link starts resolving because its target appears.
    ok(textdb(&db).args(["write", "/later.md"]), Some("# Later\n"));
    let after = ok(textdb(&db).args(["--json", "stat", "/a.md"]), None).json();
    assert_eq!(after["nlinks_broken"], 0, "the link resolves now");
    assert_eq!(after["version"], before["version"], "and /a.md was not rewritten");
    // The folder total followed it down.
    let root = ok(textdb(&db).args(["--json", "stat", "/"]), None).json();
    assert_eq!(root["nlinks_broken"], 0);
}

/// `search` and `grep` return one row shape, and a `line` always has a `version`.
#[test]
fn search_and_grep_share_one_row_shape() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kb.db");
    ok(
        textdb(&db).args(["write", "/s/doc.md"]),
        Some("# Guide\n\nintro\n\n## Errors\n\nthe rate limit is 100 per minute.\n"),
    );
    const KEYS: [&str; 7] = ["path", "version", "line", "text", "section", "score", "more"];
    let keys_of = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };

    let hits = ok(textdb(&db).args(["--json", "search", "rate", "limit"]), None).json();
    let hits = hits.as_array().unwrap();
    assert_eq!(keys_of(&hits[0]), KEYS, "search");
    assert_eq!(hits[0]["version"], 1);
    assert_eq!(hits[0]["section"], "Guide / Errors");
    assert!(hits[0]["score"].as_f64().unwrap() > 0.0, "higher is better");

    let greps = ok(textdb(&db).args(["--json", "grep", "rate"]), None).json();
    assert_eq!(keys_of(&greps.as_array().unwrap()[0]), KEYS, "grep");
    assert!(greps[0]["score"].is_null(), "grep ranks nothing");

    // `-l` and `-c` filter rows on both commands without changing the row type.
    for flag in ["-l", "-c"] {
        for cmd in ["search", "grep"] {
            let rows = ok(textdb(&db).args(["--json", cmd, flag, "rate"]), None).json();
            assert_eq!(keys_of(&rows.as_array().unwrap()[0]), ["path", "version", "matches"], "{cmd} {flag}");
        }
    }

    // `--per-file` truncation is visible in JSON, which it never was.
    ok(textdb(&db).args(["write", "/s/many.md"]), Some("rate\nrate\nrate\nrate\n"));
    let capped = ok(textdb(&db).args(["--json", "search", "--per-file", "2", "rate"]), None).json();
    let many = capped.as_array().unwrap().iter().find(|h| h["path"] == "/s/many.md").unwrap();
    assert_eq!(many["more"], 2, "two lines were held back and the row says so");
}

/// A path echoed back is the path the store knows, whatever the caller typed.
#[test]
fn paths_come_back_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kb.db");
    ok(textdb(&db).args(["write", "/n/a.md"]), Some("one\n"));
    ok(textdb(&db).args(["write", "/n/a.md"]), Some("two\n"));
    for args in [vec!["--json", "cat", "n/a.md"], vec!["--json", "diff", "n/a.md", "1", "2"]] {
        let v = ok(textdb(&db).args(&args), None).json();
        assert_eq!(v["path"], "/n/a.md", "{args:?}");
    }
}

/// The history row's nine keys, in the order the `commits` view and `textdb_history` use.
/// They used to come back in a third order here, so `SELECT *` consumed positionally swapped
/// `nbytes` and `kind` between the CLI and either engine.
#[test]
fn history_rows_are_in_the_commits_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kb.db");
    ok(textdb(&db).args(["write", "/h/a.md"]), Some("one\n"));
    ok(textdb(&db).args(["write", "/h/a.md"]), Some("one\ntwo\n"));

    const ORDER: [&str; 9] =
        ["version", "author", "ts", "message", "kind", "base_version", "nbytes", "nlines", "nwords"];
    let keys_of = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };

    let plain = ok(textdb(&db).args(["--json", "history", "/h/a.md"]), None).json();
    // `--json history` tags each row; a flag filters rows, it never changes the row type.
    let tagged: Vec<String> = std::iter::once("type".to_string()).chain(ORDER.map(str::to_string)).collect();
    assert_eq!(keys_of(&plain[0]), tagged, "history");

    let only = ok(textdb(&db).args(["--json", "history", "/h/a.md", "--versions-only"]), None).json();
    assert_eq!(keys_of(&only[0]), tagged, "history --versions-only");

    let sql = ok(
        textdb(&db).args(["--json", "sql", "SELECT * FROM textdb_history('/h/a.md') LIMIT 1"]),
        None,
    )
    .json();
    let cols: Vec<&str> = sql["columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    assert_eq!(cols, ORDER, "textdb_history");

    // The `commits` view carries the same nine after `path`, then the batch that wrote it.
    let view = ok(textdb(&db).args(["--json", "sql", "SELECT * FROM commits LIMIT 1"]), None).json();
    let cols: Vec<&str> = view["columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    let want: Vec<&str> = ["path"].into_iter().chain(ORDER).chain(["batch"]).collect();
    assert_eq!(cols, want, "commits view");
}

/// `tree` says what each folder holds all the way down, and `tree FILE` shows the one entry
/// rather than a header claiming the file's folder holds nothing.
#[test]
fn tree_counts_folders_and_prints_a_file_plainly() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kb.db");
    ok(textdb(&db).args(["write", "/t/api/one.md"]), Some("a\n"));
    ok(textdb(&db).args(["write", "/t/api/deep/two.md"]), Some("bb\n"));

    let all = ok(textdb(&db).args(["tree", "/t"]), None).stdout;
    assert!(all.starts_with("/t  (2 files, 2 folders, "), "{all}");
    assert!(all.contains("api/  (2 files, 1 folder, "), "{all}");
    assert!(all.contains("deep/  (1 file, 0 folders, "), "{all}");

    let one = ok(textdb(&db).args(["tree", "/t/api/one.md"]), None).stdout;
    assert!(!one.contains("0 files"), "{one}");
    assert!(one.contains("one.md"), "{one}");
}

/// Two syncs of one directory at once: exactly one runs, and the other fails saying so.
///
/// Without the directory lock both went ahead, each computing both sides from the same base, and
/// one line appended on disk landed twice on disk and three times in the store — with both runs
/// reporting success. Failing the second is the point: a collision a caller can see beats two
/// exit-zero runs where the second quietly found the work already done.
///
/// Ignored by default because it races two real processes: the first has to still be working when
/// the second reaches the lock, which 800 new files buy here but a faster machine might not.
/// `two_syncs_at_once_serialise` holds the lock itself and asserts the same rule without a race;
/// this one stays for `--ignored` runs, where it also checks the data after a real collision.
#[test]
#[ignore = "races two processes; run with --ignored"]
fn two_syncs_at_once_are_one_success_and_one_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    // Enough files that the first run is still going when the second starts.
    for i in 0..600 {
        std::fs::write(dir.join(format!("n{i}.md")), format!("# N {i}\n\nbody line\n")).unwrap();
    }
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);

    let note = dir.join("n5.md");
    std::fs::write(&note, "# N 5\n\nbody line\n- appended\n").unwrap();
    for i in 600..1400 {
        std::fs::write(dir.join(format!("n{i}.md")), format!("# N {i}\n\nbody line\n")).unwrap();
    }

    let both: Vec<_> = (0..2)
        .map(|_| {
            textdb(&db)
                .args(["sync", "/"])
                .arg(&dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn sync")
        })
        .collect();
    let outs: Vec<Output> = both
        .into_iter()
        .map(|c| {
            let o = c.wait_with_output().unwrap();
            Output {
                status: o.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            }
        })
        .collect();

    let codes: Vec<i32> = outs.iter().map(|o| o.status).collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 1, "exactly one should run: {codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 4).count(), 1, "exactly one should fail: {codes:?}");
    let refused = outs.iter().find(|o| o.status == 4).unwrap();
    assert!(refused.stderr.contains("already being synced by another process"), "{}", refused.stderr);
    assert!(refused.stderr.contains("--lock-timeout"), "the message should say how to queue: {}", refused.stderr);

    // The one that ran did the whole job, and nothing was doubled.
    let on_disk = std::fs::read_to_string(&note).unwrap();
    let in_store = ok(textdb(&db).args(["cat", "/n5.md"]), None).stdout;
    assert_eq!(on_disk, "# N 5\n\nbody line\n- appended\n", "disk");
    assert_eq!(in_store, on_disk, "the store and disk agree");
    assert!(!on_disk.contains("<<<<<<<") && !on_disk.contains(">>>>>>>"), "no markers: {on_disk}");
    let history = ok(textdb(&db).args(["--json", "history", "/n5.md", "--versions-only"]), None).json();
    assert_eq!(history.as_array().unwrap().len(), 2, "one disk edit is one new version: {history}");
    let files = ok(textdb(&db).args(["--json", "sql", "SELECT count(*) AS n FROM files"]), None).json();
    assert_eq!(files["rows"][0]["n"], 1400, "{files}");

    // The failure is not a dead end: run it again and the rest goes through.
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);
}

/// The same rule as the race above, with the race taken out: while the lock is held, a sync of
/// that directory fails, and once it is released the next one runs. No timing, no two processes.
#[test]
fn two_syncs_at_once_serialise() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.md"), "# A\n\nbody\n").unwrap();
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);

    // Stand in for the sync that got there first.
    let held = std::fs::OpenOptions::new().read(true).write(true).open(dir.join(".textdb").join("lock")).unwrap();
    held.try_lock().expect("hold the lock");

    std::fs::write(dir.join("a.md"), "# A\n\nbody\n- appended\n").unwrap();
    let refused = run(textdb(&db).args(["sync", "/"]).arg(&dir), None);
    assert_eq!(refused.status, 4, "stdout: {}\nstderr: {}", refused.stdout, refused.stderr);
    assert!(refused.stderr.contains("already being synced by another process"), "{}", refused.stderr);
    assert!(refused.stderr.contains("--lock-timeout"), "the message should say how to queue: {}", refused.stderr);
    // It failed before doing anything: the store still has the text from before the edit.
    assert_eq!(ok(textdb(&db).args(["cat", "/a.md"]), None).stdout, "# A\n\nbody\n");

    held.unlock().unwrap();
    drop(held);
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);
    assert_eq!(ok(textdb(&db).args(["cat", "/a.md"]), None).stdout, "# A\n\nbody\n- appended\n");
    let history = ok(textdb(&db).args(["--json", "history", "/a.md", "--versions-only"]), None).json();
    assert_eq!(history.as_array().unwrap().len(), 2, "one edit, one version: {history}");
}

/// `--lock-timeout` is the other half: a caller that would rather queue than retry.
#[test]
#[ignore = "races two processes; run with --ignored"]
fn lock_timeout_queues_behind_a_running_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..600 {
        std::fs::write(dir.join(format!("n{i}.md")), format!("# N {i}\n\nbody\n")).unwrap();
    }
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);
    for i in 600..1400 {
        std::fs::write(dir.join(format!("n{i}.md")), format!("# N {i}\n\nbody\n")).unwrap();
    }

    // Both queue, so it does not matter which reaches the lock first: neither fails, where the
    // default would have failed whichever came second.
    let both: Vec<_> = (0..2)
        .map(|_| {
            textdb(&db)
                .args(["sync", "/"])
                .arg(&dir)
                .args(["--lock-timeout", "60"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn sync")
        })
        .collect();
    let outs: Vec<_> = both.into_iter().map(|c| c.wait_with_output().unwrap()).collect();
    for o in &outs {
        assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    }
    // One did the work and the other found it done, in whichever order they got the lock.
    let said: Vec<String> = outs.iter().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).collect();
    assert_eq!(said.iter().filter(|s| s.contains("1400 unchanged")).count(), 1, "{said:?}");
    let files = ok(textdb(&db).args(["--json", "sql", "SELECT count(*) AS n FROM files"]), None).json();
    assert_eq!(files["rows"][0]["n"], 1400, "{files}");
}

/// A sync will not start while another holds the directory, and says so rather than proceeding.
#[test]
fn a_second_sync_waits_or_reports_the_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.md"), "one\n").unwrap();
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);

    // Hold the lock the way a running sync does, from this process.
    let lock = dir.join(".textdb").join("lock");
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock).unwrap();
    held.try_lock().expect("hold the lock the way a running sync does");

    let refused = run(textdb(&db).args(["sync", "/"]).arg(&dir), None);
    assert_eq!(refused.status, 4, "stdout: {}\nstderr: {}", refused.stdout, refused.stderr);
    assert!(refused.stderr.contains("being synced by another process"), "{}", refused.stderr);

    // Released, and the next sync goes through.
    held.unlock().unwrap();
    drop(held);
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);
}

/// A dry run reads only, so it neither takes the lock nor queues behind a sync that holds it.
#[test]
fn a_dry_run_does_not_queue_behind_a_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.md"), "one\n").unwrap();
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);

    let lock = dir.join(".textdb").join("lock");
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock).unwrap();
    held.try_lock().expect("hold the lock the way a running sync does");
    ok(textdb(&db).args(["sync", "/"]).arg(&dir).arg("--dry-run"), None);
}

/// `.textdb` keeps a checkout clean on its own, so nothing textdb writes beside a directory shows
/// up as untracked and the user's `.gitignore` needs no entry for it.
#[test]
fn the_textdb_directory_ignores_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.md"), "one\n").unwrap();
    ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);

    let ignore = std::fs::read_to_string(dir.join(".textdb").join(".gitignore")).unwrap();
    assert!(ignore.contains('*'), "{ignore}");
}

/// A synced directory remembers what it is paired with, so `textdb sync` needs no arguments from
/// anywhere inside it — and refuses a folder or store that contradicts the pairing, which used to
/// import the whole tree again under a second prefix without a word.
#[test]
fn a_synced_directory_remembers_its_store_and_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(dir.join("notes/deep")).unwrap();
    std::fs::write(dir.join("a.md"), "one\n").unwrap();
    std::fs::write(dir.join("notes/deep/c.md"), "three\n").unwrap();
    ok(textdb(&db).args(["sync", "/docs"]).arg(&dir), None);

    let config = std::fs::read_to_string(dir.join(".textdb").join("config")).unwrap();
    assert!(config.contains("prefix = \"/docs\""), "{config}");
    assert!(config.contains("id = \""), "{config}");

    // No arguments at all, three levels down: the pairing supplies the folder, the directory and
    // the store, so `-s` is not needed either.
    let mut bare = Command::new(env!("CARGO_BIN_EXE_textdb"));
    bare.current_dir(dir.join("notes/deep"))
        .env_remove("TEXTDB_STORE")
        .env("TEXTDB_CONFIG_DIR", tmp.path().join("config"))
        .arg("sync");
    let out = ok(&mut bare, None);
    assert!(out.stdout.contains("/docs with"), "{}", out.stdout);
    assert!(out.stdout.contains("2 unchanged"), "{}", out.stdout);

    // A different folder, and a different store, are refused by name.
    let other = run(textdb(&db).args(["sync", "/elsewhere"]).arg(&dir), None);
    assert_eq!(other.status, 6, "{}", other.stdout);
    assert!(other.stderr.contains("is paired with /docs"), "{}", other.stderr);
    let second = tmp.path().join("second.db");
    let mut from_inside = Command::new(env!("CARGO_BIN_EXE_textdb"));
    from_inside
        .current_dir(&dir)
        .env_remove("TEXTDB_STORE")
        .env("TEXTDB_CONFIG_DIR", tmp.path().join("config"))
        .arg("--store")
        .arg(&second)
        .arg("sync");
    let moved = run(&mut from_inside, None);
    assert_eq!(moved.status, 6, "{}", moved.stdout);
    assert!(moved.stderr.contains("paired with the store"), "{}", moved.stderr);

    // One argument is the folder in the store, so a lone directory is a mistake worth naming.
    let mistake = run(textdb(&db).arg("sync").arg(&dir), None);
    assert_eq!(mistake.status, 6, "{}", mistake.stdout);
    assert!(mistake.stderr.contains("is a directory on this computer"), "{}", mistake.stderr);

    // --force says so on purpose, and the pairing follows.
    ok(textdb(&db).args(["sync", "--force", "/elsewhere"]).arg(&dir), None);
    let config = std::fs::read_to_string(dir.join(".textdb").join("config")).unwrap();
    assert!(config.contains("prefix = \"/elsewhere\""), "{config}");
}

/// The store's own files are never documents, whichever way they would travel.
#[test]
fn the_store_is_left_out_of_the_directory_it_syncs() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("kb.db");
    std::fs::write(dir.join("a.md"), "one\n").unwrap();

    // `--ext '*'` takes in everything text-shaped, which is how `kb.db-wal` was offered.
    ok(textdb(&db).args(["sync", "/"]).arg(&dir).args(["--ext", "*"]), None);
    let out = ok(textdb(&db).args(["sync", "/"]).arg(&dir).args(["--ext", "*"]), None);
    assert!(out.stdout.contains("the store itself"), "{}", out.stdout);

    let paths = ok(textdb(&db).args(["ls", "-1", "-R", "/"]), None).stdout;
    assert!(paths.contains("/a.md"), "{paths}");
    for name in ["kb.db", "kb.db-wal", "kb.db-shm"] {
        assert!(!paths.contains(name), "{name} was taken in: {paths}");
    }
}

/// What a sync walked past, and the one line a hook wants instead of the whole list.
#[test]
fn sync_says_what_it_left_out_and_can_keep_quiet() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.md"), "one\n").unwrap();
    std::fs::write(dir.join("data.csv"), "x,y\n").unwrap();
    std::fs::write(dir.join("app.json"), "{}\n").unwrap();

    let first = ok(textdb(&db).args(["sync", "/"]).arg(&dir), None);
    assert!(first.stdout.contains("left out        2 files by extension"), "{}", first.stdout);
    assert!(first.stdout.contains("--ext to include them"), "{}", first.stdout);
    // A plain directory is not an Obsidian vault, so no .textdbignore is written into it and
    // nothing of sync's own counts as a change on disk.
    assert!(!dir.join(".textdbignore").exists());
    assert!(first.stdout.contains("disk 0 new"), "{}", first.stdout);

    std::fs::write(dir.join("b.md"), "two\n").unwrap();
    let quiet = ok(textdb(&db).args(["sync", "/"]).arg(&dir).arg("--quiet"), None);
    assert!(!quiet.stdout.contains("textdb new"), "{}", quiet.stdout);
    assert_eq!(quiet.stdout.lines().filter(|l| l.starts_with("synced:")).count(), 1, "{}", quiet.stdout);
    // The same two files are still left out, so it does not say so again: on a code directory
    // that line was the whole output of every hook run.
    assert!(!quiet.stdout.contains("left out"), "{}", quiet.stdout);

    // A third one appears, and it says so once.
    std::fs::write(dir.join("more.csv"), "a,b\n").unwrap();
    let changed = ok(textdb(&db).args(["sync", "/"]).arg(&dir).arg("--quiet"), None);
    assert!(changed.stdout.contains("left out        3 files by extension"), "{}", changed.stdout);
    let settled = ok(textdb(&db).args(["sync", "/"]).arg(&dir).arg("--quiet"), None);
    assert!(!settled.stdout.contains("left out"), "{}", settled.stdout);
}

/// Two directories per folder is the normal state — a person's vault and an agent's checkout —
/// and `assets` used to take whichever was synced with the folder last, silently. It now takes
/// the one the command is run from, and says which it used.
#[test]
fn assets_use_the_directory_you_are_standing_in() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let bucket = tmp.path().join("bucket");
    let one = tmp.path().join("one");
    let two = tmp.path().join("two");
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    std::fs::write(one.join("a.md"), "see ![[logo.png]]\n").unwrap();

    ok(textdb(&db).args(["assets", "stores", "--add", "team", "--root", bucket.to_str().unwrap()]), None);
    // --add creates a local root rather than leaving the first push to fail on a missing folder.
    assert!(bucket.is_dir());

    ok(textdb(&db).args(["sync", "/v"]).arg(&one), None);
    ok(textdb(&db).args(["sync", "/v"]).arg(&two), None);
    // `two` was synced last, so that is what the old rule would have picked.
    std::fs::write(one.join("logo.png"), b"\x89PNG one").unwrap();

    let mut from_one = Command::new(env!("CARGO_BIN_EXE_textdb"));
    from_one
        .current_dir(&one)
        .env_remove("TEXTDB_STORE")
        .env("TEXTDB_CONFIG_DIR", tmp.path().join("config"))
        .arg("--store")
        .arg(&db)
        .args(["assets", "push"]);
    let out = ok(&mut from_one, None);
    assert!(out.stdout.contains("pushed 1 assets"), "{}", out.stdout);
    // And it names the directory it used, which push never did.
    assert!(out.stdout.contains(one.to_str().unwrap()), "{}", out.stdout);
}

/// The sync base follows the directory's own id, so moving or renaming the directory is not a
/// re-import of everything in it under a path that has changed.
#[test]
fn a_directory_that_moves_keeps_its_sync_base() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let one = tmp.path().join("one");
    let two = tmp.path().join("two");
    std::fs::create_dir_all(&one).unwrap();
    std::fs::write(one.join("a.md"), "one\n").unwrap();
    std::fs::write(one.join("b.md"), "two\n").unwrap();
    let first = ok(textdb(&db).args(["sync", "/v"]).arg(&one), None);
    assert!(first.stdout.contains("textdb 2 new"), "{}", first.stdout);

    std::fs::rename(&one, &two).unwrap();
    let after = ok(textdb(&db).args(["sync", "/v"]).arg(&two), None);
    assert!(after.stdout.contains("2 unchanged"), "{}", after.stdout);
    assert!(after.stdout.contains("textdb 0 new"), "{}", after.stdout);
    // Said once, so a reader knows why the directory in the summary changed.
    assert!(after.stdout.contains("was ") && after.stdout.contains("at the last sync"), "{}", after.stdout);

    // And the base moved rather than being duplicated.
    let bases = ok(textdb(&db).args(["--json", "sql", "SELECT dir FROM kb_sync"]), None).json();
    assert_eq!(bases["rows"].as_array().unwrap().len(), 1, "{bases}");
}

/// The sync base is a compare-and-swap, so a run that another machine sharing the folder
/// overtook does not record its own base over theirs.
///
/// The directory lock covers one computer; this is the case it cannot see. The base is written
/// last, after the store and disk writes, so what the overtaken run wrote stays — it is versioned
/// in the store and present on disk. What the check buys is the *next* sync's starting point: it
/// reconciles against the base the other machine left rather than taking the overtaken run's view
/// as the agreed state.
#[test]
fn a_sync_another_machine_overtook_does_not_record_its_base() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..200 {
        std::fs::write(dir.join(format!("n{i}.md")), format!("# N {i}\n\nbody\n")).unwrap();
    }
    ok(textdb(&db).args(["sync", "/v"]).arg(&dir), None);

    let generation = |label: &str| -> i64 {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.query_row("SELECT generation FROM kb_sync", [], |r| r.get(0)).unwrap_or_else(|e| panic!("{label}: {e}"))
    };
    // One sync, one generation: the mechanism is live rather than stuck at zero.
    assert_eq!(generation("after the first sync"), 1);

    // Enough new work that the run takes long enough for the other machine to land inside it.
    for i in 200..1400 {
        std::fs::write(dir.join(format!("n{i}.md")), format!("# N {i}\n\nbody\n")).unwrap();
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bumping = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = conn.execute("UPDATE kb_sync SET generation = generation + 1", []);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };
    let overtaken = run(textdb(&db).args(["sync", "/v"]).arg(&dir), None);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    bumping.join().unwrap();

    assert_eq!(overtaken.status, 4, "stdout: {}\nstderr: {}", overtaken.stdout, overtaken.stderr);
    assert!(overtaken.stderr.contains("were synced by another process"), "{}", overtaken.stderr);
    // The message says what is true: the writes landed, only the base did not.
    assert!(overtaken.stderr.contains("is in the store and on disk"), "{}", overtaken.stderr);
    let files = ok(textdb(&db).args(["--json", "sql", "SELECT count(*) AS n FROM files"]), None).json();
    assert_eq!(files["rows"][0]["n"], 1400, "the run's work is in the store: {files}");

    // And a following sync succeeds, leaving both sides agreeing.
    let again = ok(textdb(&db).args(["sync", "/v"]).arg(&dir), None);
    assert!(again.stdout.contains("1400 unchanged"), "{}", again.stdout);
}

/// A sync killed while it is writing leaves every file whole: the old bytes or the new ones,
/// never a truncated one.
///
/// `write_disk` used to open the target with `truncate(true)` and write into it, so a sync a hook
/// timed out on — or any kill — left an empty note where a document had been. The bytes now go to
/// `.textdb/tmp` and are renamed over the target, which no reader can see half of.
#[test]
fn a_killed_sync_leaves_no_half_written_file() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let dir = tmp.path().join("vault");
    std::fs::create_dir_all(&dir).unwrap();
    // Big enough that writing them all takes long enough to be interrupted part-way.
    let old: String = (0..600).map(|i| format!("old line {i}\n")).collect();
    let new: String = (0..600).map(|i| format!("new line {i}\n")).collect();
    for i in 0..60 {
        std::fs::write(dir.join(format!("n{i}.md")), &old).unwrap();
    }
    ok(textdb(&db).args(["sync", "/v"]).arg(&dir), None);

    // Change every document in the store, so the next sync has to rewrite every file on disk.
    ok(
        textdb(&db).args(["sql", "--write", "UPDATE kb SET content = replace(content, 'old line', 'new line')"]),
        None,
    );

    let mut child = textdb(&db)
        .args(["sync", "/v"])
        .arg(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sync");
    std::thread::sleep(std::time::Duration::from_millis(40));
    let _ = child.kill();
    let _ = child.wait();

    // Whatever it managed, every file is one of the two whole texts.
    let mut newly = 0;
    for i in 0..60 {
        let text = std::fs::read_to_string(dir.join(format!("n{i}.md"))).unwrap();
        if text == new {
            newly += 1;
        } else {
            assert_eq!(text, old, "n{i}.md is neither the old text nor the new one ({} bytes)", text.len());
        }
    }

    // The staging files of a run that was killed do not pile up: the next sync holds the lock, so
    // anything left in .textdb/tmp is from a run that is gone, and it clears them.
    ok(textdb(&db).args(["sync", "/v"]).arg(&dir), None);
    let staging = dir.join(".textdb").join("tmp");
    let left: Vec<_> = std::fs::read_dir(&staging).map(|d| d.flatten().collect()).unwrap_or_default();
    assert!(left.is_empty(), "staging files left behind: {left:?}");
}

/// Discovery walks up, and the two ways of stopping it work.
///
/// A synced directory above the one you are in would otherwise pair your command with a store it
/// knows nothing about — which is how a misfired test came to sync this repository once, and why
/// the harness pins `TEXTDB_CEILING_DIRECTORIES`.
#[test]
fn discovery_stops_where_it_is_told_to() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let outer = tmp.path().join("outer");
    let inner = outer.join("a").join("b");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(outer.join("top.md"), "top\n").unwrap();
    ok(textdb(&db).args(["sync", "/outer"]).arg(&outer), None);

    // From inside, with nothing said: the pairing above is found.
    let sync_from = |at: &std::path::Path, env: &[(&str, &std::ffi::OsStr)]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_textdb"));
        cmd.current_dir(at)
            .env_remove("TEXTDB_STORE")
            .env_remove("TEXTDB_DIR")
            .env("TEXTDB_CONFIG_DIR", tmp.path().join("config"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.arg("sync");
        run(&mut cmd, None)
    };
    let found = sync_from(&inner, &[("TEXTDB_CEILING_DIRECTORIES", std::ffi::OsStr::new(""))]);
    assert_eq!(found.status, 0, "{}", found.stderr);
    assert!(found.stdout.contains("/outer with"), "{}", found.stdout);

    // A ceiling at the directory you are in stops the walk before it starts, so nothing is found
    // and the current directory is what gets synced — with its own folder, not the one above.
    let stopped = sync_from(&inner, &[("TEXTDB_CEILING_DIRECTORIES", inner.as_os_str())]);
    assert_eq!(stopped.status, 0, "{}", stopped.stderr);
    assert!(stopped.stdout.contains(inner.to_str().unwrap()), "{}", stopped.stdout);
    assert!(!stopped.stdout.contains("/outer with"), "{}", stopped.stdout);

    // TEXTDB_DIR names one outright, from anywhere.
    let named = sync_from(tmp.path(), &[("TEXTDB_DIR", outer.as_os_str())]);
    assert_eq!(named.status, 0, "{}", named.stderr);
    assert!(named.stdout.contains("/outer with"), "{}", named.stdout);
}

/// A folder names who changed something below it, and how many people have.
///
/// Both were the contract in `docs/shapes.md` and neither was true: `nauthors` came back 0 for
/// every folder, because the column only ever holds a file's own count, and `updated_by` was the
/// folder row's own author, which nothing sets.
#[test]
fn a_folder_carries_the_authors_below_it() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("kb.db");
    let write = |author: &str, path: &str, text: &str| {
        ok(textdb(&db).args(["-a", author, "write", path]), Some(text));
    };
    write("alice", "/notes/a/x.md", "one\n");
    write("bob", "/notes/a/y.md", "two\n");
    write("alice", "/notes/b/z.md", "three\n");

    let row = |path: &str| -> serde_json::Value {
        ok(textdb(&db).args(["--json", "stat", path]), None).json()
    };
    assert_eq!((row("/notes/a")["nauthors"].as_i64(), row("/notes/a")["updated_by"].as_str()), (Some(2), Some("bob")));
    assert_eq!((row("/notes/b")["nauthors"].as_i64(), row("/notes/b")["updated_by"].as_str()), (Some(1), Some("alice")));
    // The root counts each author once, not once per file.
    assert_eq!(row("/")["nauthors"].as_i64(), Some(2));

    // A later commit by a third author moves both.
    write("carol", "/notes/a/x.md", "one again\n");
    assert_eq!((row("/notes/a")["nauthors"].as_i64(), row("/notes/a")["updated_by"].as_str()), (Some(3), Some("carol")));
    assert_eq!(row("/")["nauthors"].as_i64(), Some(3));
    // `ls` says the same as `stat`, on the same row.
    let ls = ok(textdb(&db).args(["--json", "ls", "/notes"]), None).json();
    let a = ls.as_array().unwrap().iter().find(|e| e["path"] == "/notes/a").unwrap();
    assert_eq!((a["nauthors"].as_i64(), a["updated_by"].as_str()), (Some(3), Some("carol")));
}
