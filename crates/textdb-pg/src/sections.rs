//! Sections on Postgres: the backfill that gives an existing store the columns a newer build
//! expects. The rows themselves are written by `write_structure` at commit, and the reading
//! side is `kb.outline()` and `kb.headings()` in `lib.rs`.

use pgrx::prelude::*;
use textdb_core::storage::Result;

use crate::store::map as storage_err;

/// Fill `heading` and `heading_lc` from the stored `heading_path`, returning the row count.
///
/// Splitting happens in SQL here because Postgres has the right primitive for it —
/// `regexp_replace` anchored at the end reads the last component exactly, including when a
/// heading itself contains `" / "`. Word counts cannot be recovered without the document's
/// bytes and are left `NULL`.
pub fn backfill() -> Result<i64> {
    Spi::run(
        "UPDATE kb.section
            SET heading = regexp_replace(heading_path, '^.* / ', ''),
                heading_lc = lower(regexp_replace(heading_path, '^.* / ', ''))
          WHERE heading = ''",
    )
    .map_err(storage_err)?;
    Ok(Spi::get_one::<i64>("SELECT count(*) FROM kb.section WHERE heading <> ''")
        .unwrap_or(Some(0))
        .unwrap_or(0))
}
