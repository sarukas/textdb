//! FP — footprint over time (§7.10): corpus, Zipf edits, footprint checkpoints, maintenance.

use std::time::Instant;

use rand::{Rng, SeedableRng};

use crate::reference::Reference;
use crate::ops;
use crate::runner::Ctx;
use crate::suites::{import_corpus, line_edit, n_lines, Zipf};

pub fn footprint(ctx: &Ctx) -> anyhow::Result<()> {
    let n_files = ctx.params.usize("n_files", 1000);
    let size = ctx.params.usize("file_size", 8192);
    let n_edits = ctx.params.usize("n_edits", 5000);
    let every = ctx.params.usize("checkpoint_every", 1000);
    let mut reference = Reference::default();
    let (_, paths) = import_corpus(ctx, &mut reference, n_files, size, "/fp")?;
    let raw: usize = paths.iter().map(|p| reference.get(p).len()).sum();
    let fp0 = ctx.backend.storage_bytes().unwrap_or(0);
    ctx.cell.metric("after_0", "footprint_bytes", fp0 as f64);
    ctx.cell.metric("after_0", "footprint_over_raw", fp0 as f64 / raw.max(1) as f64);
    let zipf = Zipf::new(paths.len(), 1.0);
    let mut rng = rand::rngs::StdRng::seed_from_u64(ctx.seed ^ 0xf0);
    let mut done = 0;
    for i in 0..n_edits {
        let p = &paths[zipf.sample(&mut rng)];
        let cur = reference.get(p).to_vec();
        let idx = rng.gen_range(0..n_lines(&cur).max(1));
        if let Some((old, new)) = line_edit(&cur, idx, 1, &format!("fp{}", i)) {
            match ctx.backend.replace(p, &old, &new, None) {
                Ok(_) => {
                    reference.replace(p, &old, &new);
                    done += 1;
                }
                Err(e) => {
                    ctx.err("", "replace", &e);
                    break;
                }
            }
        }
        if (i + 1) % every == 0 {
            let fp = ctx.backend.storage_bytes().unwrap_or(0);
            let raw_now: usize = paths.iter().map(|p| reference.get(p).len()).sum();
            ctx.cell.metric(&format!("after_{}", i + 1), "footprint_bytes", fp as f64);
            ctx.cell.metric(&format!("after_{}", i + 1), "footprint_over_raw", fp as f64 / raw_now.max(1) as f64);
        }
    }
    ctx.cell.metric("", "edits", done as f64);
    let t = Instant::now();
    match ctx.op_only(ops::MAINTENANCE, || ctx.backend.maintenance()) {
        Ok(label) => {
            ctx.cell.metric("", "maintenance_s", t.elapsed().as_secs_f64());
            ctx.cell.note("", "maintenance", label);
            let fp = ctx.backend.storage_bytes().unwrap_or(0);
            let raw_now: usize = paths.iter().map(|p| reference.get(p).len()).sum();
            ctx.cell.metric("", "footprint_after_maintenance", fp as f64);
            ctx.cell.metric("", "footprint_after_maintenance_over_raw", fp as f64 / raw_now.max(1) as f64);
        }
        Err(e) => {
            ctx.err("", "maintenance", &e);
        }
    }
    ctx.set_reference(reference);
    Ok(())
}
