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
    cmd.env_remove("TEXTDB_STORE").env_remove("TEXTDB_AUTHOR").arg("--store").arg(store);
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
