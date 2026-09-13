//! XL — very large files (§7.2): create, full read, fragment read, localised replace.

use crate::backend::WriteOutcome;
use crate::gen::{GenOpts, Generator};
use crate::metrics::Latencies;
use crate::ops;
use crate::runner::Ctx;
use crate::suites::{line_edit, n_lines, size_label};

pub fn xl(ctx: &Ctx) -> anyhow::Result<()> {
    let sizes = ctx.params.list_u64("sizes", &[10 << 20]);
    let positions = ctx.params.list_f64("positions", &[0.0, 0.5, 1.0]);
    let frag_lines = ctx.params.usize("read_lines", 50);
    let edit_lines = ctx.params.usize("edit_lines", 3);
    let seq_edits = ctx.params.usize("sequential_edits", 0);
    for (si, &size) in sizes.iter().enumerate() {
        let case = size_label(size);
        let path = format!("/xl/f{}.txt", si); // plain text: content-layer cost, no structure extraction
        let mut g = Generator::new(ctx.seed.wrapping_add(si as u64));
        let mut body = g.markdown(size as usize, &GenOpts::default());
        ctx.backend.reset_counters()?;
        let fp0 = ctx.backend.storage_bytes().unwrap_or(0);
        let mut cl = Latencies::default();
        if let Err(e) = ctx.op(ops::CREATE, &mut cl, || ctx.backend.create(&path, &body)) {
            ctx.err(&case, "create", &e);
            continue;
        }
        ctx.cell.lat(&case, "create", &cl);
        ctx.cell.metric(&case, "create_bytes_written", ctx.backend.bytes_written_since_reset().unwrap_or(0) as f64);
        ctx.cell.metric(&case, "footprint_after_create", ctx.backend.storage_bytes().unwrap_or(0).saturating_sub(fp0) as f64);
        // XL-02 full read (3 reps → warm).
        let mut rl = Latencies::default();
        let mut ok = true;
        for _ in 0..3 {
            match ctx.op(ops::READ, &mut rl, || ctx.backend.read(&path)) {
                Ok(got) => ok &= got == body,
                Err(e) => {
                    ctx.err(&case, "read", &e);
                    ok = false;
                    break;
                }
            }
        }
        ctx.cell.lat(&case, "read", &rl);
        if let Some(p50) = rl.summary().first().map(|x| x.1) {
            ctx.cell.metric(&case, "read_MBps", size as f64 / (p50 / 1e6) / 1e6);
        }
        if ok {
            ctx.cell.metric(&case, "identical", 1.0);
        } else {
            ctx.cell.fail(&case, "identical", "full read differs");
        }
        // XL-03 fragment reads at positions.
        let nl = n_lines(&body);
        for &pos in &positions {
            let from = ((nl.saturating_sub(frag_lines)) as f64 * pos) as u64 + 1;
            let to = from + frag_lines as u64 - 1;
            let mut fl = Latencies::default();
            let mut good = true;
            for _ in 0..3 {
                match ctx.op(ops::READ_LINES, &mut fl, || ctx.backend.read_lines(&path, from, to)) {
                    Ok(got) => good &= got == crate::backends::fs::slice_lines(&body, from, to),
                    Err(e) => {
                        ctx.err(&case, "read_lines", &e);
                        good = false;
                        break;
                    }
                }
            }
            let c = format!("{}@{:.0}%", case, pos * 100.0);
            ctx.cell.lat(&c, "read_lines", &fl);
            if good {
                ctx.cell.metric(&c, "read_lines_identical", 1.0);
            } else {
                ctx.cell.fail(&c, "read_lines_identical", "fragment differs");
            }
        }
        // XL-04 replace 3 lines at positions; write amplification.
        for (k, &pos) in positions.iter().enumerate() {
            let nl = n_lines(&body);
            let idx = ((nl.saturating_sub(edit_lines + 1)) as f64 * pos) as usize;
            let (old, new) = match line_edit(&body, idx, edit_lines, &format!("xl{}", k)) {
                Some(x) => x,
                None => continue,
            };
            ctx.backend.reset_counters()?;
            let before = ctx.backend.storage_bytes().unwrap_or(0);
            let mut el = Latencies::default();
            let r = ctx.op(ops::REPLACE, &mut el, || ctx.backend.replace(&path, &old, &new, None));
            let c = format!("{}@{:.0}%", case, pos * 100.0);
            match r {
                Ok(WriteOutcome::Committed { .. }) | Ok(WriteOutcome::Absorbed { .. }) => {
                    body = crate::reference::splice(&body, &old, &new).unwrap();
                    let written = ctx.backend.bytes_written_since_reset().unwrap_or(0);
                    let changed = old.len().max(new.len()) as f64;
                    ctx.cell.lat(&c, "replace", &el);
                    ctx.cell.metric(&c, "replace_bytes_written", written as f64);
                    ctx.cell.metric(&c, "replace_write_amplification", written as f64 / changed);
                    ctx.cell.metric(&c, "replace_footprint_growth", ctx.backend.storage_bytes().unwrap_or(0).saturating_sub(before) as f64);
                    match ctx.backend.read(&path) {
                        Ok(got) if got == body => ctx.cell.metric(&c, "replace_identical", 1.0),
                        Ok(_) => ctx.cell.fail(&c, "replace_identical", "content differs after replace"),
                        Err(e) => {
                            ctx.err(&c, "read", &e);
                        }
                    }
                }
                Ok(o) => ctx.cell.fail(&c, "replace", &format!("{:?}", o)),
                Err(e) => {
                    ctx.err(&c, "replace", &e);
                }
            }
        }
        // XL-05 sequential replaces: footprint growth per edit.
        if seq_edits > 0 {
            let before = ctx.backend.storage_bytes().unwrap_or(0);
            let mut sl = Latencies::default();
            let mut done = 0;
            for i in 0..seq_edits {
                let nl = n_lines(&body);
                let idx = (i * 7919) % nl.max(1);
                let (old, new) = match line_edit(&body, idx, edit_lines.min(nl - idx).max(1), &format!("seq{}", i)) {
                    Some(x) => x,
                    None => continue,
                };
                match ctx.op(ops::REPLACE, &mut sl, || ctx.backend.replace(&path, &old, &new, None)) {
                    Ok(WriteOutcome::Committed { .. }) | Ok(WriteOutcome::Absorbed { .. }) => {
                        body = crate::reference::splice(&body, &old, &new).unwrap();
                        done += 1;
                    }
                    Ok(o) => {
                        ctx.cell.fail(&case, "seq_replace", &format!("{:?}", o));
                        break;
                    }
                    Err(e) => {
                        ctx.err(&case, "seq_replace", &e);
                        break;
                    }
                }
            }
            let after = ctx.backend.storage_bytes().unwrap_or(0);
            ctx.cell.lat(&case, "seq_replace", &sl);
            if done > 0 {
                ctx.cell.metric(&case, "footprint_growth_per_edit", after.saturating_sub(before) as f64 / done as f64);
            }
            // XL-06 history + read_version(v1) after the edits.
            let mut hl = Latencies::default();
            if let Ok(h) = ctx.op(ops::HISTORY, &mut hl, || ctx.backend.history(&path)) {
                ctx.cell.lat(&case, "history", &hl);
                ctx.cell.metric(&case, "versions", h.len() as f64);
                let mut vl = Latencies::default();
                if let Ok(v1) = ctx.op(ops::READ_VERSION, &mut vl, || ctx.backend.read_version(&path, h.first().copied().unwrap_or(1))) {
                    ctx.cell.lat(&case, "read_version_v1", &vl);
                    let mut g2 = Generator::new(ctx.seed.wrapping_add(si as u64));
                    let orig = g2.markdown(size as usize, &GenOpts::default());
                    if v1 == orig {
                        ctx.cell.metric(&case, "v1_identical", 1.0);
                    } else {
                        ctx.cell.fail(&case, "v1_identical", "version 1 differs from original");
                    }
                }
            } else {
                ctx.cell.na(&case, "history not available");
            }
        }
        for (k, v) in ctx.backend.extra_stats(&path).unwrap_or_default() {
            ctx.cell.metric(&case, k, v);
        }
    }
    Ok(())
}
