//! Front-matter properties: kept in `{p}property` at every commit, and asked about with the
//! query language in [`textdb_md::query`].
//!
//! The rows are what make metadata search an index seek instead of a scan. Without them a
//! question like "which notes have `status: draft`" read every `frontmatter` row and parsed
//! its JSON — measured at 37 ms over 50,000 notes, and linear, so five filters were five
//! scans. With them the same question is 2 ms, and enumerating property names — which a UI
//! does on every keystroke to autosuggest — goes from 67 ms to 0.11 ms.

use rusqlite::types::Value;
use rusqlite::{params, Connection};
use textdb_core::storage::Result;
use textdb_md::query::{Expr, Op, Prop, Term};

use crate::db::TextDb;
use crate::storage::sql_err;

/// Property rows written per statement when replacing a document's set.
const ROW_BATCH: usize = 64;

impl TextDb<'_> {
    /// Replace `file_id`'s property rows with those of `data`, the parsed front matter.
    ///
    /// Called from `write_structure`, which has already decided the structure changed; a
    /// commit that leaves front matter alone never reaches here.
    pub(crate) fn write_property_rows(&self, file_id: i64, version: i64, data: Option<&serde_json::Value>) -> Result<()> {
        self.conn
            .prepare_cached(&format!("DELETE FROM {}property WHERE file_id = ?1", self.p))
            .map_err(sql_err)?
            .execute(params![file_id])
            .map_err(sql_err)?;
        let Some(data) = data else { return Ok(()) };
        let props = textdb_md::query::flatten(data);
        insert_rows(self.conn, &self.p, file_id, version, &props)
    }

    /// Carry the property rows forward when a commit changed content but not structure.
    ///
    /// They are keyed to the document's current version, the same as `section` and `link`,
    /// so a stale `version` would hide them from every query.
    pub(crate) fn touch_property_version(&self, file_id: i64, version: i64) -> Result<()> {
        self.conn
            .prepare_cached(&format!("UPDATE {}property SET version = ?1 WHERE file_id = ?2 AND version <> ?1", self.p))
            .map_err(sql_err)?
            .execute(params![version, file_id])
            .map_err(sql_err)?;
        Ok(())
    }
}

fn insert_rows(conn: &Connection, p: &str, file_id: i64, version: i64, props: &[Prop]) -> Result<()> {
    for batch in props.chunks(ROW_BATCH) {
        let marks = vec!["(?, ?, ?, ?, ?, ?, ?, ?)"; batch.len()].join(",");
        let mut args: Vec<Value> = Vec::with_capacity(batch.len() * 8);
        for pr in batch {
            args.push(file_id.into());
            args.push(version.into());
            args.push(pr.key.clone().into());
            args.push(pr.key.to_lowercase().into());
            args.push(pr.text.clone().map(Value::Text).unwrap_or(Value::Null));
            args.push(pr.text.as_ref().map(|t| Value::Text(t.to_lowercase())).unwrap_or(Value::Null));
            args.push(pr.num.map(Value::Real).unwrap_or(Value::Null));
            args.push(pr.ord.into());
        }
        conn.prepare_cached(&format!(
            "INSERT INTO {p}property(file_id, version, key, key_lc, val_txt, val_lc, val_num, ord) VALUES {marks}"
        ))
        .map_err(sql_err)?
        .execute(rusqlite::params_from_iter(args.iter()))
        .map_err(sql_err)?;
    }
    Ok(())
}

/// Build the property rows for every document that has front matter and none yet.
///
/// Runs from `migrate`, so an existing vault answers metadata queries the first time it is
/// opened by a build that has this table, rather than silently returning nothing.
pub fn backfill(conn: &Connection, p: &str) -> Result<()> {
    let pending: Vec<(i64, i64, String)> = {
        let mut st = conn
            .prepare(&format!(
                "SELECT f.file_id, f.version, f.data FROM {p}frontmatter f
                   WHERE f.data IS NOT NULL
                     AND NOT EXISTS (SELECT 1 FROM {p}property r WHERE r.file_id = f.file_id)"
            ))
            .map_err(sql_err)?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?)))
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)?
    };
    for (file_id, version, data) in pending {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        insert_rows(conn, p, file_id, version, &textdb_md::query::flatten(&v))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Asking
// ---------------------------------------------------------------------------------------

/// A `WHERE` fragment and its arguments.
pub struct Sql {
    pub where_clause: String,
    pub args: Vec<Value>,
}

/// Compile a parsed query into a predicate over `{p}node n`.
///
/// The caller pairs this with "the document has at least one property row", which is what
/// makes the answers consistent. Without it the universe is every document, so `-status:draft`
/// returns notes with no front matter at all — they are not draft, after all — while an empty
/// query returns them too, and the two disagree with everything else the family reports. A
/// property search is over documents that have properties.
///
/// Every term becomes an `EXISTS` against the property rows of the row being tested, which is
/// what keeps `NOT` honest: `-status:archived` has to mean "no property row says archived",
/// not "some row says something else", and a join would have given the second. It also makes
/// `AND` across two properties work without a self-join, since each term is independent.
pub fn compile(p: &str, e: &Expr) -> std::result::Result<Sql, textdb_md::query::QueryError> {
    let mut args = Vec::new();
    let where_clause = build(p, e, &mut args)?;
    Ok(Sql { where_clause, args })
}

fn build(p: &str, e: &Expr, args: &mut Vec<Value>) -> std::result::Result<String, textdb_md::query::QueryError> {
    Ok(match e {
        Expr::All => "1".to_string(),
        Expr::Not(inner) => format!("NOT ({})", build(p, inner, args)?),
        Expr::And(parts) | Expr::Or(parts) => {
            let joiner = if matches!(e, Expr::And(_)) { " AND " } else { " OR " };
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                out.push(build(p, part, args)?);
            }
            format!("({})", out.join(joiner))
        }
        Expr::Term(t) => term_sql(p, t, args),
    })
}

fn term_sql(p: &str, t: &Term, args: &mut Vec<Value>) -> String {
    // The condition and its arguments are built together and appended in statement order.
    // `!=` emits two subqueries, so pushing the key before knowing that would have put the
    // bound values out of step with their placeholders.
    let mut vals: Vec<Value> = Vec::new();
    let cond = match t.op {
        Op::Exists => String::new(),
        Op::Eq | Op::Ne => {
            vals.push(Value::Text(t.value.to_lowercase()));
            // Numbers compare numerically as well, so `year:2026` finds a value stored as
            // 2026.0 and `version:1.0` finds "1".
            let mut c = " AND (val_lc = ?".to_string();
            if let Some(n) = t.num {
                vals.push(Value::Real(n));
                c.push_str(" OR val_num = ?");
            }
            c.push(')');
            c
        }
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let sym = match t.op {
                Op::Lt => "<",
                Op::Le => "<=",
                Op::Gt => ">",
                _ => ">=",
            };
            match t.num {
                // A numeric bound compares against the numeric column, so 10 is above 9.
                Some(n) => {
                    vals.push(Value::Real(n));
                    format!(" AND val_num {} ?", sym)
                }
                // A non-numeric bound is a text comparison, which is what makes dates work:
                // ISO-8601 sorts correctly as text.
                None => {
                    vals.push(Value::Text(t.value.to_lowercase()));
                    format!(" AND val_lc {} ?", sym)
                }
            }
        }
        Op::Prefix => {
            vals.push(Value::Text(format!("{}%", escape_like(&t.value.to_lowercase()))));
            " AND val_lc LIKE ? ESCAPE '\\'".to_string()
        }
        Op::Contains => {
            vals.push(Value::Text(format!("%{}%", escape_like(&t.value.to_lowercase()))));
            " AND val_lc LIKE ? ESCAPE '\\'".to_string()
        }
    };
    // `n.id IN (SELECT …)` rather than `EXISTS (… WHERE file_id = n.id …)`.
    //
    // They mean the same thing and cost wildly different amounts: the correlated EXISTS made
    // SQLite walk every node and probe by `file_id`, measured at 3.28 s over 5,000 notes,
    // while the list subquery seeks `property(key_lc, val_lc)` once and looks the nodes up by
    // rowid — 0.002 s, the same plan for each term, so two properties cost two seeks.
    let subquery = format!("SELECT file_id FROM {p}property WHERE key_lc = ?{cond}");
    let key = || Value::Text(t.key.to_lowercase());
    // `!=` means "has the property, but not with this value" rather than "lacks it": a note
    // with no `status` at all is not a note whose status is not draft.
    if matches!(t.op, Op::Ne) {
        args.push(key()); // the "has it at all" subquery
        args.push(key()); // the "has it with this value" subquery
        args.extend(vals);
        return format!("(n.id IN (SELECT file_id FROM {p}property WHERE key_lc = ?) AND n.id NOT IN ({subquery}))");
    }
    args.push(key());
    args.extend(vals);
    format!("n.id IN ({subquery})")
}

/// `%` and `_` are wildcards in `LIKE`; a property value that contains them means them.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// One property name and how much of the vault uses it.
#[derive(Clone, Debug)]
pub struct KeyRow {
    pub key: String,
    /// Documents carrying it — not rows, so a note with three tags counts once.
    pub docs: i64,
    /// Distinct values it takes.
    pub values: i64,
    /// `number`, `text` or `mixed`, so a UI can offer `>` only where it means something.
    pub kind: String,
}

/// One value a property takes, and how often.
#[derive(Clone, Debug)]
pub struct ValueRow {
    pub value: Option<String>,
    pub docs: i64,
}

/// One document a query matched.
#[derive(Clone, Debug)]
pub struct HitRow {
    pub path: String,
    pub nbytes: i64,
    pub updated_at: String,
    /// The document's whole front matter, so a result list can show any column without a
    /// second round trip per row — which is what a table view would otherwise do.
    pub frontmatter: Option<String>,
}

impl TextDb<'_> {
    /// Property names in use, most-used first.
    ///
    /// `prefix` filters by what the user has typed, which is the autosuggest call: it runs on
    /// every keystroke, so it is an index range over `(key, val_txt)` rather than a scan.
    pub fn property_keys(&self, prefix: &str, limit: usize) -> Result<Vec<KeyRow>> {
        let lower = prefix.to_lowercase();
        let (lo, hi) = prefix_range(&lower);
        // `docs` is a count of documents, and an account's count has to be of documents it can
        // read — otherwise `meta keys` reports the existence of files it may not see, one
        // integer at a time.
        let (vis_sql, vis_args) = self.visible_for("n.path", 4);
        let mut st = self
            .conn
            .prepare_cached(&format!(
                "SELECT r.key,
                        count(DISTINCT r.file_id),
                        count(DISTINCT r.val_txt),
                        CASE WHEN count(r.val_num) = 0 THEN 'text'
                             WHEN count(r.val_num) = count(r.val_txt) THEN 'number'
                             ELSE 'mixed' END
                   FROM {p}property r
                   JOIN {p}node n ON n.id = r.file_id AND n.deleted_at IS NULL
                  WHERE (?1 = '' OR (r.key_lc >= ?2 AND r.key_lc < ?3)) AND ({vis})
                  GROUP BY r.key
                  ORDER BY count(DISTINCT r.file_id) DESC, r.key
                  LIMIT ?4",
                p = self.p,
                vis = vis_sql,
            ))
            .map_err(sql_err)?;
        let mut args: Vec<Value> = vec![
            Value::Text(lower),
            Value::Text(lo),
            Value::Text(hi),
            Value::Integer(limit as i64),
        ];
        args.extend(vis_args);
        let rows = st
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok(KeyRow {
                    key: r.get(0)?,
                    docs: r.get(1)?,
                    values: r.get(2)?,
                    kind: r.get(3)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }

    /// The values one property takes, most-used first; `prefix` narrows them as above.
    pub fn property_values(&self, key: &str, prefix: &str, limit: usize) -> Result<Vec<ValueRow>> {
        let lower = prefix.to_lowercase();
        let (lo, hi) = prefix_range(&lower);
        let (vis_sql, vis_args) = self.visible_for("n.path", 5);
        let mut st = self
            .conn
            .prepare_cached(&format!(
                "SELECT r.val_txt, count(DISTINCT r.file_id)
                   FROM {p}property r
                   JOIN {p}node n ON n.id = r.file_id AND n.deleted_at IS NULL
                  WHERE r.key_lc = ?1
                    AND (?2 = '' OR (r.val_lc >= ?3 AND r.val_lc < ?4)) AND ({vis})
                  GROUP BY r.val_txt
                  ORDER BY count(DISTINCT r.file_id) DESC, r.val_txt
                  LIMIT ?5",
                p = self.p,
                vis = vis_sql,
            ))
            .map_err(sql_err)?;
        let mut args: Vec<Value> = vec![
            Value::Text(key.to_lowercase()),
            Value::Text(lower),
            Value::Text(lo),
            Value::Text(hi),
            Value::Integer(limit as i64),
        ];
        args.extend(vis_args);
        let rows = st
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok(ValueRow {
                    value: r.get(0)?,
                    docs: r.get(1)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }

    /// Documents matching a query, under `folder`.
    pub fn property_find(&self, query: &str, folder: &str, limit: usize) -> Result<Vec<HitRow>> {
        let expr = textdb_md::query::parse(query).map_err(|e| textdb_core::TextdbError::InvalidEdit(e.to_string()))?;
        let sql = compile(&self.p, &expr).map_err(|e| textdb_core::TextdbError::InvalidEdit(e.to_string()))?;
        let folder = &self.store_path(folder)?;
        let (lo, hi) = crate::db::subtree_bounds(folder).unwrap_or(("/".into(), "0".into()));
        // Every placeholder is a bare `?`, and the folder bounds come *after* the compiled
        // clause in the statement text. Numbering some of them `?N` while the compiled clause
        // used bare `?` made SQLite continue its own numbering past the highest explicit
        // index, so the terms silently bound to parameters that were never passed.
        // Bare `?` throughout, so the visible-set predicate uses bare ones too and its arguments
        // go in before the limit — the order in the statement text is the order here.
        let (vis_sql, vis_args) = self.visible_bare("n.path");
        let mut args = sql.args;
        args.push(Value::Text(folder.to_string()));
        args.push(Value::Text(folder.to_string()));
        args.push(Value::Text(lo));
        args.push(Value::Text(hi));
        args.extend(vis_args);
        args.push(Value::Integer(limit as i64));
        let mut st = self
            .conn
            .prepare_cached(&format!(
                "SELECT n.path, n.nbytes, n.updated_at,
                        (SELECT f.data FROM {p}frontmatter f WHERE f.file_id = n.id AND f.version = n.version)
                   FROM {p}node n
                  WHERE n.deleted_at IS NULL AND n.kind = 1
                    AND EXISTS (SELECT 1 FROM {p}property u WHERE u.file_id = n.id)
                    AND ({where_clause})
                    AND (? = '/' OR n.path = ? OR (n.path >= ? AND n.path < ?))
                    AND ({vis})
                  ORDER BY n.path
                  LIMIT ?",
                p = self.p,
                where_clause = sql.where_clause,
                vis = vis_sql,
            ))
            .map_err(sql_err)?;
        let rows = st
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok(HitRow {
                    path: r.get(0)?,
                    nbytes: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    updated_at: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    frontmatter: r.get(3)?,
                })
            })
            .map_err(sql_err)?;
        let rows: Vec<HitRow> = rows.collect::<rusqlite::Result<_>>().map_err(sql_err)?;
        // Each hit names a document, so each path is the caller's. The predicate above already
        // dropped what it cannot see, so nothing here should fall out; `filter_map` rather than
        // an unwrap because a row that somehow did would otherwise be a panic on a read.
        Ok(rows
            .into_iter()
            .filter_map(|mut h| {
                h.path = self.view_path(&h.path)?;
                Some(h)
            })
            .collect())
    }
}

/// `[lo, hi)` covering everything that starts with `p`, so a prefix match is an index range
/// rather than `LIKE 'p%'` — which SQLite can only use when it can prove the collation, and
/// cannot at all once `lower()` is involved.
fn prefix_range(p: &str) -> (String, String) {
    let mut hi = p.to_string();
    // Step the last character up: the successor of "pro" is "prp", so `[pro, prp)` is exactly
    // the keys beginning with "pro".
    while let Some(c) = hi.pop() {
        if let Some(next) = char::from_u32(c as u32 + 1) {
            hi.push(next);
            return (p.to_string(), hi);
        }
    }
    // Empty prefix: the caller's `?1 = ''` branch matches everything, and these are ignored.
    (String::new(), String::new())
}
