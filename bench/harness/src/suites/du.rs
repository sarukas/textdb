//! DU — durability and crash (§7.9). Killing a writer or the server mid-write needs a
//! separate supervisor process per backend (and `dm-flakey` for DU-03); this harness runs
//! all backends in one process, so the cells are recorded as N/A with the reason rather
//! than silently skipped.

use crate::runner::Ctx;

pub fn durability(ctx: &Ctx) -> anyhow::Result<()> {
    ctx.cell.na(
        "",
        "kill -9 mid-write needs an out-of-process supervisor; not implemented in the POC harness",
    );
    Ok(())
}
