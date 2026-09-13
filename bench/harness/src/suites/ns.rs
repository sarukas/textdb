//! NS — namespace (§7.8): wide folders, deep paths, subtree rename, delete + history,
//! path characters.

use std::time::Instant;

use crate::gen::{GenOpts, Generator};
use crate::metrics::Latencies;
use crate::ops;
use crate::runner::Ctx;

pub fn namespace(ctx: &Ctx) -> anyhow::Result<()> {
    let variant = ctx.params.str("variant", "many_in_folder");
    let mut g = Generator::new(ctx.seed);
    match variant.as_str() {
        "many_in_folder" => {
            let n = ctx.params.usize("n_files", 10_000);
            let body = g.markdown(512, &GenOpts::default());
            let mut cl = Latencies::default();
            for i in 0..n {
                if let Err(e) = ctx.op(ops::CREATE, &mut cl, || ctx.backend.create(&format!("/wide/f{:07}.md", i), &body)) {
                    ctx.err("", "create", &e);
                    return Ok(());
                }
            }
            ctx.cell.lat("", "create", &cl);
            let last: Latencies = Latencies {
                samples: cl.samples[cl.samples.len().saturating_sub(100)..].to_vec(),
            };
            ctx.cell.lat("", "create_last100", &last);
            let mut ll = Latencies::default();
            match ctx.op(ops::LIST, &mut ll, || ctx.backend.list("/wide")) {
                Ok(l) => {
                    let files = l.iter().filter(|e| !e.is_dir).count();
                    if files == n {
                        ctx.cell.metric("", "list_count_ok", 1.0);
                    } else {
                        ctx.cell.fail("", "list_count_ok", &format!("{} of {}", files, n));
                    }
                }
                Err(e) => {
                    ctx.err("", "list", &e);
                }
            }
            ctx.cell.lat("", "list", &ll);
        }
        "depth" => {
            let depth = ctx.params.usize("depth", 1000);
            let body = g.markdown(512, &GenOpts::default());
            let path = format!("{}/leaf.md", "/a".repeat(depth));
            let mut cl = Latencies::default();
            match ctx.op(ops::CREATE, &mut cl, || ctx.backend.create(&path, &body)) {
                Ok(_) => {
                    ctx.cell.lat("", "create", &cl);
                    let mut rl = Latencies::default();
                    match ctx.op(ops::READ, &mut rl, || ctx.backend.read(&path)) {
                        Ok(got) if got == body => ctx.cell.metric("", "identical", 1.0),
                        Ok(_) => ctx.cell.fail("", "identical", "differs"),
                        Err(e) => {
                            ctx.err("", "read", &e);
                        }
                    }
                    ctx.cell.lat("", "read", &rl);
                    let mut ll = Latencies::default();
                    let parent = "/a".repeat(depth);
                    if let Err(e) = ctx.op(ops::LIST, &mut ll, || ctx.backend.list(&parent)) {
                        ctx.err("", "list", &e);
                    }
                    ctx.cell.lat("", "list", &ll);
                }
                Err(e) => {
                    ctx.cell.note("", "create", &format!("rejected: {}", e));
                    ctx.cell.metric("", "rejected", 1.0);
                }
            }
        }
        "rename_folder" => {
            let counts = ctx.params.list_u64("descendants", &[1000, 10_000]);
            for &n in &counts {
                let case = format!("{}", n);
                let src = format!("/src{}", n);
                let dst = format!("/dst{}", n);
                let mut bodies = Vec::new();
                for i in 0..n as usize {
                    let body = g.markdown(1024, &GenOpts::default());
                    let p = format!("{}/d{}/f{:06}.md", src, i % 20, i);
                    if let Err(e) = ctx.backend.create(&p, &body) {
                        ctx.err(&case, "create", &e);
                        return Ok(());
                    }
                    bodies.push((format!("/d{}/f{:06}.md", i % 20, i), body));
                }
                // One extra version on the first file so version preservation is observable.
                let first = format!("{}{}", src, bodies[0].0);
                let _ = ctx.backend.replace(&first, &bodies[0].1[..8].to_vec(), b"RENAMED!", None);
                let versions_before = ctx.backend.history(&first).map(|h| h.len()).unwrap_or(0);
                let mut rl = Latencies::default();
                match ctx.op(ops::RENAME, &mut rl, || ctx.backend.rename(&src, &dst)) {
                    Ok(()) => {
                        ctx.cell.lat(&case, "rename", &rl);
                        // Oracle: all paths updated, contents unchanged, versions preserved.
                        let t = Instant::now();
                        let listed = ctx.backend.list(&dst).map(|l| l.iter().filter(|e| !e.is_dir).count()).unwrap_or(0);
                        ctx.cell.metric(&case, "list_after_s", t.elapsed().as_secs_f64());
                        if listed == n as usize {
                            ctx.cell.metric(&case, "paths_updated", 1.0);
                        } else {
                            ctx.cell.fail(&case, "paths_updated", &format!("{} of {} under new path", listed, n));
                        }
                        let old_left = ctx.backend.list(&src).map(|l| l.len()).unwrap_or(0);
                        if old_left > 0 {
                            ctx.cell.fail(&case, "old_paths_gone", &format!("{} entries still under old path", old_left));
                        }
                        let mut bad = 0;
                        for (rel, body) in bodies.iter().skip(1).step_by((n as usize / 50).max(1)) {
                            match ctx.backend.read(&format!("{}{}", dst, rel)) {
                                Ok(got) if &got == body => {}
                                _ => bad += 1,
                            }
                        }
                        if bad == 0 {
                            ctx.cell.metric(&case, "contents_unchanged", 1.0);
                        } else {
                            ctx.cell.fail(&case, "contents_unchanged", &format!("{} sampled files differ", bad));
                        }
                        match ctx.backend.history(&format!("{}{}", dst, bodies[0].0)) {
                            Ok(h) if h.len() == versions_before && versions_before >= 2 => ctx.cell.metric(&case, "versions_preserved", 1.0),
                            Ok(h) => ctx.cell.fail(&case, "versions_preserved", &format!("{} versions after, {} before", h.len(), versions_before)),
                            Err(e) => {
                                ctx.err(&case, "history", &e);
                            }
                        }
                    }
                    Err(e) => {
                        ctx.err(&case, "rename", &e);
                    }
                }
            }
        }
        "delete_then_read_version" => {
            let n = ctx.params.usize("descendants", 1000);
            let mut first_body = Vec::new();
            for i in 0..n {
                let body = g.markdown(1024, &GenOpts::default());
                if i == 0 {
                    first_body = body.clone();
                }
                if let Err(e) = ctx.backend.create(&format!("/del/d{}/f{:06}.md", i % 10, i), &body) {
                    ctx.err("", "create", &e);
                    return Ok(());
                }
            }
            let mut dl = Latencies::default();
            if let Err(e) = ctx.op(ops::DELETE, &mut dl, || ctx.backend.delete("/del")) {
                ctx.err("", "delete", &e);
                return Ok(());
            }
            ctx.cell.lat("", "delete", &dl);
            let gone = ctx.backend.list("/del").map(|l| l.is_empty()).unwrap_or(true);
            if gone {
                ctx.cell.metric("", "deleted", 1.0);
            } else {
                ctx.cell.fail("", "deleted", "entries still listed");
            }
            let mut vl = Latencies::default();
            match ctx.op(ops::READ_VERSION, &mut vl, || ctx.backend.read_version("/del/d0/f000000.md", 1)) {
                Ok(got) if got == first_body => {
                    ctx.cell.metric("", "read_version_after_delete", 1.0);
                    ctx.cell.lat("", "read_version", &vl);
                }
                Ok(_) => ctx.cell.fail("", "read_version_after_delete", "content differs"),
                Err(e) => {
                    ctx.err("", "read_version_after_delete", &e);
                }
            }
        }
        "path_chars" => {
            let long_name = "n".repeat(255);
            let deep: String = (0..60).map(|i| format!("/segment-{:02}-{}", i, "x".repeat(50))).collect();
            let cases: Vec<(&str, String)> = vec![
                ("spaces", "/dir with spaces/file name.md".into()),
                ("dots", "/a.b.c/file.name.with.dots.md".into()),
                ("unicode", "/ąčę/一二三/😀 note.md".into()),
                ("255byte_name", format!("/long/{}", long_name)),
                ("4KiB_path", format!("{}/leaf.md", deep)),
                ("percent_underscore", "/100%_done/a_b%c.md".into()),
            ];
            let body = g.markdown(600, &GenOpts::default());
            for (label, path) in cases {
                match ctx.backend.create(&path, &body) {
                    Ok(_) => match ctx.backend.read(&path) {
                        Ok(got) if got == body => ctx.cell.metric(label, "identical", 1.0),
                        Ok(_) => ctx.cell.fail(label, "identical", "differs"),
                        Err(e) => {
                            ctx.err(label, "read", &e);
                        }
                    },
                    Err(e) => {
                        ctx.cell.note(label, "create", &format!("rejected: {}", e));
                        ctx.cell.metric(label, "rejected", 1.0);
                    }
                }
            }
        }
        other => anyhow::bail!("unknown namespace variant {}", other),
    }
    Ok(())
}
