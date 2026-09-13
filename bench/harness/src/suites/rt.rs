//! RT — round-trip accuracy (§7.1).

use crate::gen::Generator;
use crate::metrics::Latencies;
use crate::ops;
use crate::runner::Ctx;
use crate::suites::{charset, line_ending, opts, size_label};

pub fn roundtrip(ctx: &Ctx) -> anyhow::Result<()> {
    let sizes = ctx.params.list_u64("sizes", &[0, 1, 511, 512, 513, 4095, 4096, 4097, 1024, 102_400, 1_048_576]);
    let endings = ctx.params.list_str("line_endings", &["lf"]);
    let charsets = ctx.params.list_str("charsets", &["mixed"]);
    let mut create_all = Latencies::default();
    let mut read_all = Latencies::default();
    let mut i = 0;
    for cs in &charsets {
        for le in &endings {
            for &size in &sizes {
                i += 1;
                let case = format!("{}/{}/{}", size_label(size), le, cs);
                let mut g = Generator::new(ctx.seed.wrapping_add(i));
                let body = g.markdown(size as usize, &opts(charset(cs), line_ending(le)));
                let path = format!("/rt/{}/{}/f{}.md", cs, le, i);
                let mut lat = Latencies::default();
                match ctx.op(ops::CREATE, &mut lat, || ctx.backend.create(&path, &body)) {
                    Ok(_) => {}
                    Err(e) => {
                        ctx.err(&case, "create", &e);
                        continue;
                    }
                }
                create_all.extend(&lat);
                let mut rl = Latencies::default();
                match ctx.op(ops::READ, &mut rl, || ctx.backend.read(&path)) {
                    Ok(got) => {
                        if got == body {
                            ctx.cell.metric(&case, "identical", 1.0);
                        } else {
                            ctx.cell.fail(&case, "identical", &format!("{} bytes expected, {} got", body.len(), got.len()));
                        }
                    }
                    Err(e) => {
                        ctx.err(&case, "read", &e);
                    }
                }
                read_all.extend(&rl);
                ctx.cell.lat(&case, "create", &lat);
                ctx.cell.lat(&case, "read", &rl);
            }
        }
    }
    ctx.cell.lat("all", "create", &create_all);
    ctx.cell.lat("all", "read", &read_all);
    Ok(())
}
