use rand::{Rng, SeedableRng};
use textdb_sqlite::TextDb;

fn wchar() -> u64 {
    std::fs::read_to_string("/proc/self/io").unwrap().lines().find(|l| l.starts_with("wchar:")).unwrap()[6..].trim().parse().unwrap()
}

fn run(label: &str, extractor: bool, fts: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;").unwrap();
    let mut db = TextDb::open(&conn, "kb_").unwrap();
    if !extractor { db.extractor = None; }
    if !fts {
        conn.execute_batch("DROP TABLE kb_fts; CREATE TABLE kb_fts(rowid INTEGER PRIMARY KEY, text TEXT);").unwrap();
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(1);
    let mut body = String::new();
    for i in 0..1500 { if i % 30 == 0 { body.push_str(&format!("## Heading {}\n\n", i)); } body.push_str(&format!("line {} {}\n", i, "word ".repeat(rng.gen_range(3..15)))); }
    db.create("/a.md", body.as_bytes(), None, None).unwrap();
    let mut reference = body.clone();
    let w0 = wchar();
    let n = 200;
    for i in 0..n {
        let lines: Vec<&str> = reference.split_inclusive('\n').collect();
        let idx = rng.gen_range(0..lines.len());
        let old = lines[idx].to_string();
        if reference.matches(&old).count() != 1 { continue; }
        let new = format!("edit{} {}\n", i, "x".repeat(rng.gen_range(0..40)));
        db.edit("/a.md", old.as_bytes(), new.as_bytes(), None).unwrap();
        reference = reference.replacen(&old, &new, 1);
    }
    let w1 = wchar();
    let wal = std::fs::metadata(format!("{}-wal", path.display())).map(|m| m.len()).unwrap_or(0);
    let dbsz = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let (chunks, nodes, commits, _, chunk_bytes) = db.stats().unwrap();
    eprintln!("{label}: {} edits, wrote {} KB ({} KB/edit); db {} KB wal {} KB; chunks {} ({} KB) nodes {} commits {}",
        n, (w1 - w0) / 1024, (w1 - w0) / 1024 / n, dbsz / 1024, wal / 1024, chunks, chunk_bytes / 1024, nodes, commits);
}

#[test]
fn write_amplification_breakdown() {
    run("full        ", true, true);
    run("no extractor", false, true);
    run("no fts      ", true, false);
    run("neither     ", false, false);
}
