//! LL — very long lines (§7.3).

use crate::backend::WriteOutcome;
use crate::gen::{Charset, Generator};
use crate::metrics::Latencies;
use crate::ops;
use crate::runner::Ctx;
use crate::suites::size_label;

pub fn long_lines(ctx: &Ctx) -> anyhow::Result<()> {
    let variant = ctx.params.str("variant", "single_line");
    let sizes = ctx.params.list_u64("sizes", &[1 << 20]);
    for (si, &size) in sizes.iter().enumerate() {
        let case = size_label(size);
        let path = format!("/ll/{}-{}.txt", variant, si);
        let mut g = Generator::new(ctx.seed.wrapping_add(si as u64));
        let mut body = match variant.as_str() {
            "multibyte" => {
                // Single line of mixed Unicode with 4-byte code points.
                let mut v = Vec::new();
                let words = ["😀", "🚀", "ąž", "数据", "x", "Ø"];
                let mut i = 0usize;
                while v.len() < size as usize {
                    v.extend_from_slice(words[i % words.len()].as_bytes());
                    v.push(b' ');
                    i += 1;
                }
                v
            }
            _ => g.single_line(size as usize, Charset::Ascii),
        };
        let mut cl = Latencies::default();
        if let Err(e) = ctx.op(ops::CREATE, &mut cl, || ctx.backend.create(&path, &body)) {
            ctx.err(&case, "create", &e);
            continue;
        }
        ctx.cell.lat(&case, "create", &cl);
        let mut rl = Latencies::default();
        match ctx.op(ops::READ, &mut rl, || ctx.backend.read(&path)) {
            Ok(got) if got == body => ctx.cell.metric(&case, "identical", 1.0),
            Ok(_) => ctx.cell.fail(&case, "identical", "differs"),
            Err(e) => {
                ctx.err(&case, "read", &e);
            }
        }
        ctx.cell.lat(&case, "read", &rl);
        // LL-03 read_lines(1,1) must return the whole line.
        let mut ll = Latencies::default();
        match ctx.op(ops::READ_LINES, &mut ll, || ctx.backend.read_lines(&path, 1, 1)) {
            Ok(got) if got == body => ctx.cell.metric(&case, "read_lines_whole_line", 1.0),
            Ok(got) => ctx.cell.fail(&case, "read_lines_whole_line", &format!("{} of {} bytes", got.len(), body.len())),
            Err(e) => {
                ctx.err(&case, "read_lines", &e);
            }
        }
        ctx.cell.lat(&case, "read_lines_1_1", &ll);
        // LL-02 / LL-06 replace in the middle (10 bytes; multibyte crosses a code point).
        let mid = body.len() / 2;
        let (old, new) = if variant == "multibyte" {
            // Pick a span that starts inside a multi-byte sequence boundary-aligned to chars
            // but replaces across two characters.
            let s = String::from_utf8_lossy(&body).into_owned();
            let mut idx = mid;
            while !s.is_char_boundary(idx) {
                idx -= 1;
            }
            let mut end = idx + 1;
            let mut chars = 0;
            while chars < 3 && end < s.len() {
                if s.is_char_boundary(end) {
                    chars += 1;
                }
                end += 1;
            }
            while !s.is_char_boundary(end) {
                end += 1;
            }
            (body[idx..end].to_vec(), "Ω✓".as_bytes().to_vec())
        } else {
            (body[mid..mid + 10].to_vec(), b"<REPLACED>".to_vec())
        };
        if crate::reference::find_unique(&body, &old).is_none() {
            ctx.cell.note(&case, "replace", "skipped: middle span not unique");
            continue;
        }
        let leaves_before = leaf_hashes(ctx, &path);
        ctx.backend.reset_counters()?;
        let mut el = Latencies::default();
        match ctx.op(ops::REPLACE, &mut el, || ctx.backend.replace(&path, &old, &new, None)) {
            Ok(WriteOutcome::Committed { .. }) | Ok(WriteOutcome::Absorbed { .. }) => {
                body = crate::reference::splice(&body, &old, &new).unwrap();
                ctx.cell.lat(&case, "replace", &el);
                let written = ctx.backend.bytes_written_since_reset().unwrap_or(0);
                ctx.cell.metric(&case, "replace_bytes_written", written as f64);
                ctx.cell.metric(&case, "replace_write_amplification", written as f64 / old.len().max(new.len()) as f64);
                match ctx.backend.read(&path) {
                    Ok(got) if got == body => {
                        ctx.cell.metric(&case, "replace_identical", 1.0);
                        if variant == "multibyte" && std::str::from_utf8(&got).is_err() {
                            ctx.cell.fail(&case, "utf8_intact", "backend corrupted UTF-8");
                        } else if variant == "multibyte" {
                            ctx.cell.metric(&case, "utf8_intact", 1.0);
                        }
                    }
                    Ok(_) => ctx.cell.fail(&case, "replace_identical", "differs after replace"),
                    Err(e) => {
                        ctx.err(&case, "read", &e);
                    }
                }
                if let (Some(b), Some(a)) = (leaves_before, leaf_hashes(ctx, &path)) {
                    let changed = a.iter().filter(|h| !b.contains(h)).count();
                    ctx.cell.metric(&case, "leaves_changed", changed as f64);
                    ctx.cell.metric(&case, "leaves_total", a.len() as f64);
                }
            }
            Ok(o) => ctx.cell.fail(&case, "replace", &format!("{:?}", o)),
            Err(e) => {
                ctx.err(&case, "replace", &e);
            }
        }
    }
    Ok(())
}

/// textdb-sqlite leaf hashes (None for other backends).
///
/// Matched on the engine rather than the whole id, so the delegated twin
/// (`textdb-sqlite@account`) is not silently left without the measurement. It reads the node
/// table directly, which speaks store paths, so the account's path is translated on the way in.
pub fn leaf_hashes(ctx: &Ctx, path: &str) -> Option<Vec<textdb_core::Hash>> {
    let (engine, delegated) = crate::backends::delegate::split(ctx.backend.id());
    if engine != "textdb-sqlite" {
        return None;
    }
    let path = crate::backends::delegate::store_path(delegated, path);
    let conn = rusqlite::Connection::open(ctx.work.join("textdb.db")).ok()?;
    let db = textdb_sqlite::TextDb::attach(&conn, "kb_", true);
    let n = db.node_by_path(&path).ok()??;
    let st = textdb_sqlite::SqliteStorage::new(&conn, "kb_");
    Some(textdb_core::leaves(&st, &n.root?).ok()?.into_iter().map(|l| l.hash).collect())
}
