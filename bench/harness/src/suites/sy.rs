//! SY — sync: reconciling a store folder with a directory on disk, both ways.
//!
//! The operation an Obsidian vault or a git checkout actually performs, and the one nothing
//! else in the matrix was measuring. No baseline has an equivalent — `fs` *is* a directory,
//! and the `sql-text-*` stores have no working copy to reconcile with — so those cells are
//! `N/A`, as in the MD family.
//!
//! The question that matters is not the first import, which happens once. It is the **no-op
//! sync**: the one a watcher or a save hook runs constantly, where nothing has changed and
//! the honest answer is "nothing to do". If that is O(corpus) rather than O(changes), then
//! syncing a large vault is expensive no matter how little moved, and `unchanged_per_file`
//! is the number that says so.

use crate::backend::SyncStats;
use crate::gen::{GenOpts, Generator};
use crate::metrics::Latencies;
use crate::ops;
use crate::runner::Ctx;

pub fn sync(ctx: &Ctx) -> anyhow::Result<()> {
    let n = ctx.params.usize("n_files", 500);
    let size = ctx.params.usize("file_size", 4096);
    let changed = ctx.params.usize("changed_files", 10).min(n);
    let prefix = "/vault";

    // The directory deliberately does **not** live under `--work`.
    //
    // `--work` defaults to `bench/data`, inside this repository, and `sync` notices when the
    // directory it is given sits in a git checkout: it attributes changes to git authors and
    // skips what `.gitignore` excludes. `bench/data` is ignored, so every file was skipped
    // and the first sync reported moving nothing — a cell that measured the ignore rules
    // rather than sync. A temporary directory outside any checkout measures plain sync; the
    // git-aware path is a different test and would have to set up a real repository.
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            ctx.cell.fail("", "setup", &e.to_string());
            return Ok(());
        }
    };
    let dir = tmp.path().join("syncdir");

    // A directory of markdown, written once and then edited in place by the cases below.
    let mut g = Generator::new(ctx.seed);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        ctx.cell.fail("", "setup", &e.to_string());
        return Ok(());
    }
    let mut bodies = Vec::with_capacity(n);
    for i in 0..n {
        let body = g.markdown(size, &GenOpts::default());
        if let Err(e) = std::fs::write(dir.join(format!("f{:05}.md", i)), &body) {
            ctx.cell.fail("", "setup", &e.to_string());
            return Ok(());
        }
        bodies.push(body);
    }

    // 1. First sync: every file crosses into the store. Reported per file, since the whole
    //    point of comparing it with the cases below is cost per unit of work.
    let mut first = Latencies::default();
    let stats = match ctx.op(ops::SYNC, &mut first, || ctx.backend.sync_dir(prefix, &dir)) {
        Ok(s) => s,
        Err(e) => {
            ctx.err("", "sync", &e);
            return Ok(());
        }
    };
    ctx.cell.lat("first", "sync", &first);
    if stats.to_store as usize == n {
        ctx.cell.metric("first", "to_store", stats.to_store as f64);
    } else {
        ctx.cell.fail("first", "to_store", &format!("{} of {}", stats.to_store, n));
    }
    per_file(ctx, "first", &first, n);

    // 2. No-op: nothing changed on either side. The steady state.
    let mut noop = Latencies::default();
    for _ in 0..ctx.params.usize("noop_reps", 3) {
        match ctx.op(ops::SYNC, &mut noop, || ctx.backend.sync_dir(prefix, &dir)) {
            Ok(s) => {
                if s != SyncStats::default() {
                    ctx.cell.fail("noop", "nothing_moved", &format!("{:?}", s));
                    break;
                }
            }
            Err(e) => {
                ctx.err("noop", "sync", &e);
                return Ok(());
            }
        }
    }
    ctx.cell.lat("noop", "sync", &noop);
    per_file(ctx, "noop", &noop, n);
    // The headline: a sync with nothing to do, divided by corpus size. A store that has to
    // re-read every file to find that out scales with the vault; one that does not, does not.
    if let (Some(a), Some(b)) = (noop.p50(), first.p50()) {
        if b > 0.0 {
            ctx.cell.metric("noop", "vs_first_pct", a / b * 100.0);
        }
    }

    // 3. Incremental: a few files changed on disk, the rest untouched. What a save hook sees.
    for (i, body) in bodies.iter().enumerate().take(changed) {
        let mut next = body.clone();
        next.extend_from_slice(format!("\n\nEdited on disk {:05}.\n", i).as_bytes());
        if std::fs::write(dir.join(format!("f{:05}.md", i)), &next).is_err() {
            break;
        }
    }
    let mut inc = Latencies::default();
    match ctx.op(ops::SYNC, &mut inc, || ctx.backend.sync_dir(prefix, &dir)) {
        Ok(s) => {
            if s.to_store as usize == changed {
                ctx.cell.metric("incremental", "to_store", s.to_store as f64);
            } else {
                ctx.cell.fail("incremental", "to_store", &format!("{} of {}", s.to_store, changed));
            }
        }
        Err(e) => {
            ctx.err("incremental", "sync", &e);
            return Ok(());
        }
    }
    ctx.cell.lat("incremental", "sync", &inc);

    // 4. The other direction: the store changed, disk did not.
    let mut back = Latencies::default();
    let mut wrote = 0;
    for i in 0..changed {
        let p = format!("{}/f{:05}.md", prefix, i);
        if ctx.backend.append(&p, b"\nAppended in the store.\n").is_ok() {
            wrote += 1;
        }
    }
    match ctx.op(ops::SYNC, &mut back, || ctx.backend.sync_dir(prefix, &dir)) {
        Ok(s) => {
            if s.to_disk >= wrote {
                ctx.cell.metric("to_disk", "files", s.to_disk as f64);
            } else {
                ctx.cell.fail("to_disk", "files", &format!("{} of {}", s.to_disk, wrote));
            }
        }
        Err(e) => {
            ctx.err("to_disk", "sync", &e);
        }
    }
    ctx.cell.lat("to_disk", "sync", &back);

    // 5. Both sides changed the same file in different places, which is the case sync exists
    //    for: a three-way merge rather than one side winning.
    let mut merge = Latencies::default();
    let mut expect = 0;
    for i in 0..changed {
        let path = dir.join(format!("f{:05}.md", i));
        let Ok(cur) = std::fs::read(&path) else { continue };
        // Disk edits the top, the store appends to the bottom: disjoint, so this must merge
        // cleanly rather than conflict.
        let mut next = format!("Disk touched the first line {:05}.\n", i).into_bytes();
        next.extend_from_slice(&cur);
        if std::fs::write(&path, &next).is_err() {
            continue;
        }
        if ctx.backend.append(&format!("{}/f{:05}.md", prefix, i), b"\nStore touched the end.\n").is_ok() {
            expect += 1;
        }
    }
    match ctx.op(ops::SYNC, &mut merge, || ctx.backend.sync_dir(prefix, &dir)) {
        Ok(s) => {
            ctx.cell.metric("merge", "merged", s.merged as f64);
            ctx.cell.metric("merge", "conflicted", s.conflicted as f64);
            // Edits in different parts of a file are what a three-way merge is for; if these
            // come back as conflicts the merge is not doing its job, however fast it is.
            if s.merged + s.conflicted >= expect && s.conflicted == 0 {
                ctx.cell.metric("merge", "disjoint_edits_merged", 1.0);
            } else {
                ctx.cell
                    .fail("merge", "disjoint_edits_merged", &format!("{} merged, {} conflicted, {} expected", s.merged, s.conflicted, expect));
            }
        }
        Err(e) => {
            ctx.err("merge", "sync", &e);
        }
    }
    ctx.cell.lat("merge", "sync", &merge);

    // 6. Deletes propagate: a file removed on disk is deleted in the store.
    let mut del = Latencies::default();
    let mut removed = 0;
    for i in 0..changed {
        if std::fs::remove_file(dir.join(format!("f{:05}.md", i))).is_ok() {
            removed += 1;
        }
    }
    match ctx.op(ops::SYNC, &mut del, || ctx.backend.sync_dir(prefix, &dir)) {
        Ok(s) => {
            if s.to_store as usize >= removed {
                ctx.cell.metric("delete", "propagated", s.to_store as f64);
            } else {
                ctx.cell.fail("delete", "propagated", &format!("{} of {}", s.to_store, removed));
            }
        }
        Err(e) => {
            ctx.err("delete", "sync", &e);
        }
    }
    ctx.cell.lat("delete", "sync", &del);
    Ok(())
}

/// Cost per file in the corpus, which is what makes the cases comparable with each other.
fn per_file(ctx: &Ctx, case: &str, l: &Latencies, n: usize) {
    if let Some(p) = l.p50() {
        if n > 0 {
            ctx.cell.metric(case, "us_per_file", p / n as f64);
        }
    }
}
