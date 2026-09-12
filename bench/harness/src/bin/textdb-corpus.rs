//! Real-corpus round trip (spec §9.4, RT-06/07): import a directory tree of files into a
//! textdb SQLite store, export it back, and report versions after a second import.
//!
//! ```text
//! textdb-corpus import <dir> <db>     # upsert every file under <dir> as /<relative path>
//! textdb-corpus export <db> <outdir>  # write every file back
//! textdb-corpus versions <db>         # count commits with version > 1 (must be 0 after re-import)
//! ```

use std::path::Path;

fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, std::path::PathBuf)>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_dir() {
            if p.file_name().map_or(false, |n| n == ".git") {
                continue;
            }
            walk(&p, root, out)?;
        } else {
            let rel = format!("/{}", p.strip_prefix(root).unwrap().to_string_lossy());
            out.push((rel, p));
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("import") => {
            let dir = Path::new(&args[2]);
            let conn = textdb_sqlite::open(&args[3])?;
            conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');")?;
            let db = textdb_sqlite::TextDb::attach(&conn, "kb_", true);
            let mut files = Vec::new();
            walk(dir, dir, &mut files)?;
            let t = std::time::Instant::now();
            let (mut created, mut updated, mut unchanged) = (0, 0, 0);
            for (rel, p) in &files {
                let body = std::fs::read(p)?;
                let r = db.upsert(rel, &body, Some("import")).map_err(|e| anyhow::anyhow!("{}: {}", rel, e))?;
                match r.kind {
                    textdb_core::CommitKind::NoOp => unchanged += 1,
                    _ if r.version == 1 => created += 1,
                    _ => updated += 1,
                }
            }
            println!(
                "imported {} files in {:.1}s: {} created, {} updated, {} unchanged",
                files.len(),
                t.elapsed().as_secs_f64(),
                created,
                updated,
                unchanged
            );
        }
        Some("export") => {
            let conn = textdb_sqlite::open(&args[2])?;
            let db = textdb_sqlite::TextDb::attach(&conn, "kb_", true);
            let out = Path::new(&args[3]);
            let mut n = 0;
            for (path, content) in db.export("/").map_err(|e| anyhow::anyhow!("{}", e))? {
                let target = out.join(path.trim_start_matches('/'));
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, content)?;
                n += 1;
            }
            println!("exported {} files", n);
        }
        Some("versions") => {
            let conn = textdb_sqlite::open(&args[2])?;
            let n: i64 = conn.query_row("SELECT count(*) FROM kb_commit WHERE version > 1", [], |r| r.get(0))?;
            println!("commits with version > 1: {}", n);
        }
        _ => eprintln!("usage: textdb-corpus import <dir> <db> | export <db> <outdir> | versions <db>"),
    }
    Ok(())
}
