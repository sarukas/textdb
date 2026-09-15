//! Front-matter properties on Postgres: the same rows and the same query language as the
//! SQLite binding, so one query text means the same thing on either.
//!
//! `jsonb` can be searched without these rows, but only by scanning. Measured over 50,000
//! notes, containment ran 9-12 ms and `jsonb_object_keys` — the query behind "what properties
//! does this vault use?", which a UI runs on every keystroke — took 1,007 ms.

use pgrx::prelude::*;
use textdb_core::storage::Result;
use textdb_md::query::{Expr, Op, Prop, Term};

use crate::store::map as storage_err;

/// Rows per statement when replacing a document's property set.
const ROW_BATCH: usize = 256;

/// Replace `file_id`'s property rows with those of `data`, the parsed front matter.
pub fn write_rows(file_id: i64, version: i64, data: Option<&serde_json::Value>) -> Result<()> {
    Spi::run_with_args("DELETE FROM kb.property WHERE file_id = $1", &[file_id.into()]).map_err(storage_err)?;
    let Some(data) = data else { return Ok(()) };
    insert(file_id, version, &textdb_md::query::flatten(data))
}

/// Carry the rows forward when a commit changed content but not structure: they are keyed to
/// the document's current version, so a stale one would hide them from every query.
pub fn touch_version(file_id: i64, version: i64) -> Result<()> {
    Spi::run_with_args(
        "UPDATE kb.property SET version = $1 WHERE file_id = $2 AND version <> $1",
        &[version.into(), file_id.into()],
    )
    .map_err(storage_err)?;
    Ok(())
}

fn insert(file_id: i64, version: i64, props: &[Prop]) -> Result<()> {
    for batch in props.chunks(ROW_BATCH) {
        // Unnested arrays rather than a statement per row: a document with a dozen properties
        // would otherwise pay a dozen round trips through SPI on every commit.
        let keys: Vec<&str> = batch.iter().map(|p| p.key.as_str()).collect();
        let keys_lc: Vec<String> = batch.iter().map(|p| p.key.to_lowercase()).collect();
        let txt: Vec<Option<&str>> = batch.iter().map(|p| p.text.as_deref()).collect();
        let txt_lc: Vec<Option<String>> = batch.iter().map(|p| p.text.as_ref().map(|t| t.to_lowercase())).collect();
        let nums: Vec<Option<f64>> = batch.iter().map(|p| p.num).collect();
        let ords: Vec<i64> = batch.iter().map(|p| p.ord).collect();
        Spi::run_with_args(
            "INSERT INTO kb.property(file_id, version, key, key_lc, val_txt, val_lc, val_num, ord) \
             SELECT $1, $2, * FROM unnest($3::text[], $4::text[], $5::text[], $6::text[], $7::float8[], $8::bigint[])",
            &[
                file_id.into(),
                version.into(),
                keys.into(),
                keys_lc.into(),
                txt.into(),
                txt_lc.into(),
                nums.into(),
                ords.into(),
            ],
        )
        .map_err(storage_err)?;
    }
    Ok(())
}

/// Build the rows for every document that has front matter and none yet.
///
/// The extension creates its schema fresh, so this is for a store whose documents were
/// written by a build without the table — it runs from `kb.migrate`.
pub fn backfill() -> Result<()> {
    let pending: Vec<(i64, i64, String)> = Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT f.file_id, f.version, f.data::text FROM kb.frontmatter f
                  WHERE f.data IS NOT NULL
                    AND NOT EXISTS (SELECT 1 FROM kb.property r WHERE r.file_id = f.file_id)",
                None,
                &[],
            )
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for r in rows {
            let id: Option<i64> = r.get(1).map_err(storage_err)?;
            let v: Option<i64> = r.get(2).map_err(storage_err)?;
            let d: Option<String> = r.get(3).map_err(storage_err)?;
            let (Some(id), Some(v), Some(d)) = (id, v, d) else { continue };
            out.push((id, v, d));
        }
        Ok::<_, textdb_core::TextdbError>(out)
    })?;
    for (file_id, version, data) in pending {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        insert(file_id, version, &textdb_md::query::flatten(&v))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Asking
// ---------------------------------------------------------------------------------------

/// Compile a parsed query into a predicate over `kb.node n`.
///
/// Every term becomes `n.id IN (SELECT file_id FROM kb.property WHERE …)`, the same shape the
/// SQLite binding settled on: it seeks `(key_lc, val_lc)` once per term instead of walking
/// every node and probing by `file_id`.
pub fn compile(e: &Expr) -> std::result::Result<(String, Vec<Arg>), textdb_md::query::QueryError> {
    let mut args: Vec<Arg> = Vec::new();
    let where_clause = build(e, &mut args);
    Ok((where_clause, args))
}

/// One bound value, kept typed so the caller can hand Postgres the right thing.
#[derive(Clone, Debug)]
pub enum Arg {
    Text(String),
    Num(f64),
}

fn build(e: &Expr, args: &mut Vec<Arg>) -> String {
    match e {
        Expr::All => "true".to_string(),
        Expr::Not(inner) => format!("NOT ({})", build(inner, args)),
        Expr::And(parts) | Expr::Or(parts) => {
            let joiner = if matches!(e, Expr::And(_)) { " AND " } else { " OR " };
            let out: Vec<String> = parts.iter().map(|p| build(p, args)).collect();
            format!("({})", out.join(joiner))
        }
        Expr::Term(t) => term_sql(t, args),
    }
}

fn term_sql(t: &Term, args: &mut Vec<Arg>) -> String {
    // Postgres numbers its placeholders, so the shape has to be decided before any text is
    // emitted: `!=` needs two subqueries and therefore shifts every value one slot along.
    // The values are collected first, then the SQL is written once with the right numbers.
    let mut vals: Vec<Arg> = Vec::new();
    match t.op {
        Op::Exists => {}
        Op::Eq | Op::Ne => {
            vals.push(Arg::Text(t.value.to_lowercase()));
            if let Some(num) = t.num {
                vals.push(Arg::Num(num));
            }
        }
        Op::Lt | Op::Le | Op::Gt | Op::Ge => match t.num {
            Some(num) => vals.push(Arg::Num(num)),
            None => vals.push(Arg::Text(t.value.to_lowercase())),
        },
        Op::Prefix => vals.push(Arg::Text(format!("{}%", escape_like(&t.value.to_lowercase())))),
        Op::Contains => vals.push(Arg::Text(format!("%{}%", escape_like(&t.value.to_lowercase())))),
    }

    // `v` is the slot of the first value; the key sits just before it.
    let cond = |v: usize| -> String {
        match t.op {
            Op::Exists => String::new(),
            Op::Eq | Op::Ne => {
                // A number matches either form, so `year:2026` finds a value stored as 2026.0
                // and `version:1.0` finds "1".
                if t.num.is_some() {
                    format!(" AND (val_lc = ${} OR val_num = ${})", v, v + 1)
                } else {
                    format!(" AND val_lc = ${}", v)
                }
            }
            Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                let sym = match t.op {
                    Op::Lt => "<",
                    Op::Le => "<=",
                    Op::Gt => ">",
                    _ => ">=",
                };
                // A numeric bound compares numerically, so 10 is above 9; anything else
                // compares as text, which is what makes ISO-8601 dates sort correctly.
                let col = if t.num.is_some() { "val_num" } else { "val_lc" };
                format!(" AND {} {} ${}", col, sym, v)
            }
            Op::Prefix | Op::Contains => format!(" AND val_lc LIKE ${} ESCAPE '\\'", v),
        }
    };

    let base = args.len() + 1;
    let key = Arg::Text(t.key.to_lowercase());
    // `!=` means "has the property, but not with this value" rather than "lacks it": a note
    // with no `status` at all is not a note whose status is not draft.
    if matches!(t.op, Op::Ne) {
        args.push(key.clone());
        args.push(key);
        args.extend(vals);
        return format!(
            "(n.id IN (SELECT file_id FROM kb.property WHERE key_lc = ${has}) \
              AND n.id NOT IN (SELECT file_id FROM kb.property WHERE key_lc = ${eq}{cond}))",
            has = base,
            eq = base + 1,
            cond = cond(base + 2)
        );
    }
    args.push(key);
    args.extend(vals);
    format!(
        "n.id IN (SELECT file_id FROM kb.property WHERE key_lc = ${key}{cond})",
        key = base,
        cond = cond(base + 1)
    )
}

/// `%` and `_` are wildcards in `LIKE`; a value that contains them means them.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}
