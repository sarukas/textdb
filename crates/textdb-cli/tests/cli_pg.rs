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
    let name = format!("textdb_cli_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
    let mut c = postgres::Client::connect(&admin, postgres::NoTls).expect("connect to TEXTDB_TEST_PG");
    c.batch_execute(&format!("CREATE DATABASE {name}")).expect("create a test database");
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
    let history = ok(textdb(&db).args(["--json", "history", "--versions-only", "/archive/a.md"]), None).json();
    let messages: Vec<&str> = history.as_array().unwrap().iter().map(|c| c["message"].as_str().unwrap_or("")).collect();
    assert_eq!(messages, ["first", "second"]);
}
