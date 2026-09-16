//! `CREATE VIRTUAL TABLE kb USING textdb(store='kb_')` (spec §7.1) and the eponymous
//! table-valued functions `textdb_ls`, `textdb_search`, `textdb_history`, `textdb_export`,
//! `textdb_feed`, `textdb_hunks`, `textdb_chunks`.

use std::borrow::Cow;
use std::ffi::CStr;
use std::os::raw::c_int;

use rusqlite::ffi;
use rusqlite::types::{Value, ValueRef};
use rusqlite::vtab::{
    dequote, parameter, Context, CreateVTab, Filters, IndexConstraintOp,
    IndexInfo, Inserts, Module, UpdateVTab, Updates, VTab, VTabConnection, VTabCursor, VTabKind,
};
use rusqlite::{Connection, Error, Result};
use textdb_core::TextdbError;

use crate::db::{normalize_path, parent_of, TextDb, DEFAULT_PREFIX};

pub fn map_err(e: TextdbError) -> Error {
    match &e {
        TextdbError::Conflict(c) => Error::ModuleError(format!(
            "TX001 conflict: {}",
            serde_json::to_string(c).unwrap_or_default()
        )),
        _ => Error::ModuleError(format!("{} {}", e.code(), e)),
    }
}

fn bytes_value(b: Vec<u8>) -> Value {
    match String::from_utf8(b) {
        Ok(s) => Value::Text(s),
        Err(e) => Value::Blob(e.into_bytes()),
    }
}

fn value_bytes(v: Value) -> Option<Vec<u8>> {
    match v {
        Value::Text(t) => Some(t.into_bytes()),
        Value::Blob(b) => Some(b),
        Value::Integer(i) => Some(i.to_string().into_bytes()),
        Value::Real(f) => Some(f.to_string().into_bytes()),
        Value::Null => None,
    }
}

fn value_str(v: Value) -> Option<String> {
    value_bytes(v).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn value_i64(v: Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(i),
        Value::Real(f) => Some(f as i64),
        Value::Text(t) => t.trim().parse().ok(),
        _ => None,
    }
}

fn ref_i64(v: ValueRef<'_>) -> Option<i64> {
    match v {
        ValueRef::Integer(i) => Some(i),
        ValueRef::Real(f) => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// kb virtual table
// ---------------------------------------------------------------------------

const KB_SCHEMA: &CStr = c"CREATE TABLE x(id INTEGER, path TEXT, name TEXT, parent_path TEXT, kind TEXT, content TEXT, version INTEGER, nbytes INTEGER, nlines INTEGER, updated_at TEXT, updated_by TEXT, base_version INTEGER HIDDEN, author TEXT HIDDEN, message TEXT HIDDEN)";

const COL_ID: c_int = 0;
const COL_PATH: c_int = 1;
const COL_CONTENT: c_int = 5;
const COL_BASE_VERSION: usize = 11;
const COL_AUTHOR: usize = 12;
const COL_MESSAGE: usize = 13;

/// `idx_num` values agreed between `best_index` and `filter`.
const IDX_SCAN: c_int = 0;
const IDX_PATH_EQ: c_int = 1;
const IDX_ID_EQ: c_int = 2;
/// Range over `path`, with the two bits below saying which bounds were supplied.
const IDX_PATH_RANGE: c_int = 4;
const IDX_RANGE_LOWER: c_int = 1;
const IDX_RANGE_UPPER: c_int = 2;

#[repr(C)]
pub struct KbTab {
    base: ffi::sqlite3_vtab,
    db: *mut ffi::sqlite3,
    prefix: String,
    /// A long-lived non-owning handle whose statement cache outlives one `filter` call.
    ///
    /// `filter` runs on every read through the table and used to `prepare` its statement
    /// each time: 9.2 us against 2.2 us from the cache, which was 74% of the gap to the
    /// plain-text baseline on an 8 KiB document. Caching the statements on a `Connection`
    /// built per call achieves nothing, because the cache dies with the call.
    ///
    /// Putting the handle here is safe in both directions. `Connection::from_handle` marks
    /// it not owned, so dropping it never calls `sqlite3_close` — it cannot outlive or
    /// interfere with the connection SQLite gave us. And the cached statements are
    /// finalized when this struct is dropped, which is `xDisconnect`: `sqlite3_close` calls
    /// `disconnectAllVtab` *before* it checks for unfinalized statements, precisely so that
    /// "the v-table implementation may be storing some prepared statements internally".
    /// An earlier attempt at this was reverted for leaving the database file unreleasable;
    /// `closing_a_connection_with_a_kb_table_releases_the_file` is the regression test it
    /// did not have.
    ///
    /// Shared rather than private so the scalar SQL functions, which get a throwaway handle
    /// per call and cannot safely keep one of their own, can prepare through it too — see
    /// `storage::shared`.
    conn: std::rc::Rc<Connection>,
}

impl KbTab {
    /// The table's own handle. Use this wherever a statement is worth caching.
    fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl Drop for KbTab {
    fn drop(&mut self) {
        // `xDisconnect`, which `sqlite3_close` runs before it looks for unfinalized
        // statements. Releasing here is what keeps the cached statements from outliving the
        // window in which finalizing them is still free.
        crate::storage::release_shared_conn(self.db);
    }
}

fn parse_prefix(args: &[&[u8]]) -> Result<String> {
    let mut prefix = DEFAULT_PREFIX.to_string();
    for a in args {
        let (k, v) = parameter(a)?;
        if k == "store" || k == "prefix" {
            prefix = dequote(&v).to_string();
        }
    }
    Ok(prefix)
}

unsafe impl<'vtab> VTab<'vtab> for KbTab {
    type Aux = ();
    type Cursor = KbCursor<'vtab>;

    fn connect(
        db: &mut VTabConnection,
        _aux: Option<&()>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        args: &[&[u8]],
    ) -> Result<(Cow<'static, CStr>, Self)> {
        let prefix = parse_prefix(&args[3.min(args.len())..])?;
        let handle = unsafe { db.handle() };
        let conn = unsafe { crate::storage::register_shared_conn(handle) }?;
        Ok((
            Cow::Borrowed(KB_SCHEMA),
            KbTab {
                base: ffi::sqlite3_vtab::default(),
                db: handle,
                prefix,
                conn,
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> Result<bool> {
        let mut eq: Option<(usize, c_int)> = None; // (constraint, idx_num 1 or 2)
        let mut lower: Option<usize> = None;
        let mut upper: Option<usize> = None;
        for (i, c) in info.constraints().enumerate() {
            if !c.is_usable() {
                continue;
            }
            match c.operator() {
                IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ => {
                    if c.column() == COL_PATH {
                        eq = Some((i, IDX_PATH_EQ));
                    } else if c.column() == COL_ID || c.column() == -1 {
                        // Keep a path equality if one was already found: it is the narrower
                        // of the two and the only one that needs no rowid lookup.
                        if eq.is_none() {
                            eq = Some((i, IDX_ID_EQ));
                        }
                    }
                }
                // A bound on `path` becomes a seek over `{p}node_path`, which is what makes
                // `WHERE path >= '/notes/' AND path < '/notes0'` — a subtree listing — cost
                // the size of the subtree instead of the size of the table.
                IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_GE | IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_GT
                    if c.column() == COL_PATH && lower.is_none() =>
                {
                    lower = Some(i);
                }
                IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LE | IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LT
                    if c.column() == COL_PATH && upper.is_none() =>
                {
                    upper = Some(i);
                }
                _ => {}
            }
        }
        if let Some((i, idx_num)) = eq {
            let mut u = info.constraint_usage(i);
            u.set_argv_index(1);
            u.set_omit(true);
            info.set_estimated_cost(1.0);
            info.set_estimated_rows(1);
            info.set_idx_num(idx_num);
            return Ok(true);
        }
        if lower.is_some() || upper.is_some() {
            let mut idx_num = IDX_PATH_RANGE;
            let mut argv = 1;
            if let Some(i) = lower {
                idx_num |= IDX_RANGE_LOWER;
                let mut u = info.constraint_usage(i);
                u.set_argv_index(argv);
                // Deliberately not omitted: the bound is widened to `>=` / `<=` regardless
                // of whether the caller wrote a strict comparison, so the range is always a
                // superset and SQLite has to apply the exact test itself.
                u.set_omit(false);
                argv += 1;
            }
            if let Some(i) = upper {
                idx_num |= IDX_RANGE_UPPER;
                let mut u = info.constraint_usage(i);
                u.set_argv_index(argv);
                u.set_omit(false);
            }
            info.set_estimated_cost(1_000.0);
            info.set_estimated_rows(500);
            info.set_idx_num(idx_num);
            return Ok(true);
        }
        info.set_estimated_cost(100_000.0);
        info.set_estimated_rows(50_000);
        info.set_idx_num(IDX_SCAN);
        Ok(true)
    }

    fn open(&'vtab mut self) -> Result<KbCursor<'vtab>> {
        Ok(KbCursor {
            base: ffi::sqlite3_vtab_cursor::default(),
            rows: Vec::new(),
            i: 0,
            // Shared borrow of the table: the cursor reads its prefix and, more to the
            // point, prepares through the table's connection so the statement cache
            // survives the call. Several cursors can be open at once, which is why this is
            // a shared reference and why `prepare_cached` taking `&self` matters.
            tab: &*self,
        })
    }
}

impl CreateVTab<'_> for KbTab {
    const KIND: VTabKind = VTabKind::Default;

    fn create(
        db: &mut VTabConnection,
        aux: Option<&()>,
        module_name: &[u8],
        database_name: &[u8],
        table_name: &[u8],
        args: &[&[u8]],
    ) -> Result<(Cow<'static, CStr>, Self)> {
        let (schema, tab) = Self::connect(db, aux, module_name, database_name, table_name, args)?;
        TextDb::open(tab.conn(), &tab.prefix).map_err(map_err)?;
        Ok((schema, tab))
    }

    fn destroy(&self) -> Result<()> {
        self.conn().execute_batch(&crate::schema::drop_sql(&self.prefix))
    }
}

impl UpdateVTab<'_> for KbTab {
    fn delete(&mut self, arg: ValueRef<'_>) -> Result<()> {
        let id = ref_i64(arg).ok_or_else(|| Error::ModuleError("bad rowid".into()))?;
        let conn = self.conn();
        let db = TextDb::attach(&conn, &self.prefix, false);
        let n = db
            .node_by_id(id)
            .map_err(map_err)?
            .ok_or_else(|| Error::ModuleError(format!("TX003 not found: id {}", id)))?;
        db.delete(&n.path).map_err(map_err)
    }

    fn insert(&mut self, args: &Inserts<'_>) -> Result<i64> {
        let conn = self.conn();
        let db = TextDb::attach(&conn, &self.prefix, false);
        let path = value_str(args.get::<Value>(2 + COL_PATH as usize)?)
            .ok_or_else(|| Error::ModuleError("TX004 path is required".into()))?;
        let path = normalize_path(&path).map_err(map_err)?;
        let kind = value_str(args.get::<Value>(2 + 4)?).unwrap_or_else(|| "file".into());
        let content = value_bytes(args.get::<Value>(2 + COL_CONTENT as usize)?);
        let author = value_str(args.get::<Value>(2 + COL_AUTHOR)?);
        let message = value_str(args.get::<Value>(2 + COL_MESSAGE)?);
        if kind == "folder" || (content.is_none() && kind != "file") {
            return db.ensure_folder(&path).map_err(map_err);
        }
        let body = content.unwrap_or_default();
        // INSERT OR REPLACE / upsert semantics: existing path with content → update.
        if db.node_by_path(&path).map_err(map_err)?.is_some() {
            db.update_content(&path, &body, None, author.as_deref(), message.as_deref())
                .map_err(map_err)?;
        } else {
            db.create(&path, &body, author.as_deref(), message.as_deref())
                .map_err(map_err)?;
        }
        let n = db.node_by_path(&path).map_err(map_err)?.unwrap();
        Ok(n.id)
    }

    fn update(&mut self, args: &Updates<'_>) -> Result<()> {
        let conn = self.conn();
        let db = TextDb::attach(&conn, &self.prefix, false);
        let id = value_i64(args.get::<Value>(0)?).ok_or_else(|| Error::ModuleError("bad rowid".into()))?;
        let n = db
            .node_by_id(id)
            .map_err(map_err)?
            .ok_or_else(|| Error::ModuleError(format!("TX003 not found: id {}", id)))?;
        let mut path = n.path.clone();
        if let Some(new_path) = value_str(args.get::<Value>(2 + COL_PATH as usize)?) {
            let new_path = normalize_path(&new_path).map_err(map_err)?;
            if new_path != path {
                let author = value_str(args.get::<Value>(2 + COL_AUTHOR)?);
                db.rename_by(&path, &new_path, author.as_deref()).map_err(map_err)?;
                path = new_path;
            }
        }
        if n.kind == 1 && !args.no_change(2 + COL_CONTENT as usize) {
            if let Some(content) = value_bytes(args.get::<Value>(2 + COL_CONTENT as usize)?) {
                let base = value_i64(args.get::<Value>(2 + COL_BASE_VERSION)?).map(|v| v as u64);
                let author = value_str(args.get::<Value>(2 + COL_AUTHOR)?);
                let message = value_str(args.get::<Value>(2 + COL_MESSAGE)?);
                db.update_content(&path, &content, base, author.as_deref(), message.as_deref())
                    .map_err(map_err)?;
            }
        }
        Ok(())
    }
}

struct KbRow {
    id: i64,
    path: String,
    name: String,
    kind: i64,
    root: Option<textdb_core::Hash>,
    version: i64,
    nbytes: Option<i64>,
    nlines: Option<i64>,
    updated_at: String,
    updated_by: Option<String>,
}

#[repr(C)]
pub struct KbCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    rows: Vec<KbRow>,
    i: usize,
    tab: &'vtab KbTab,
}

unsafe impl VTabCursor for KbCursor<'_> {
    fn filter(&mut self, idx_num: c_int, _idx_str: Option<&str>, args: &Filters<'_>) -> Result<()> {
        let prefix = &self.tab.prefix;
        let cols = "id, path, name, kind, root, version, nbytes, nlines, updated_at, updated_by";
        let (sql, params): (String, Vec<Value>) = match idx_num {
            IDX_PATH_EQ => (
                format!("SELECT {} FROM {}node WHERE path = ?1 AND deleted_at IS NULL", cols, prefix),
                vec![Value::Text(
                    normalize_path(&value_str(args.get::<Value>(0)?).unwrap_or_default()).unwrap_or_default(),
                )],
            ),
            IDX_ID_EQ => (
                format!("SELECT {} FROM {}node WHERE id = ?1 AND deleted_at IS NULL", cols, prefix),
                vec![Value::Integer(value_i64(args.get::<Value>(0)?).unwrap_or(-1))],
            ),
            n if n & IDX_PATH_RANGE != 0 => {
                // The bounds are widened to `>=` / `<=` whatever the caller wrote; `omit` was
                // left false in `best_index` so SQLite re-applies the exact comparison.
                let mut where_ = String::from("deleted_at IS NULL AND path <> '/'");
                let mut vals = Vec::new();
                let mut arg = 0usize;
                if n & IDX_RANGE_LOWER != 0 {
                    where_.push_str(&format!(" AND path >= ?{}", vals.len() + 1));
                    vals.push(Value::Text(value_str(args.get::<Value>(arg)?).unwrap_or_default()));
                    arg += 1;
                }
                if n & IDX_RANGE_UPPER != 0 {
                    where_.push_str(&format!(" AND path <= ?{}", vals.len() + 1));
                    vals.push(Value::Text(value_str(args.get::<Value>(arg)?).unwrap_or_default()));
                }
                (
                    format!("SELECT {} FROM {}node WHERE {} ORDER BY path", cols, prefix, where_),
                    vals,
                )
            }
            _ => (
                format!(
                    "SELECT {} FROM {}node WHERE deleted_at IS NULL AND path <> '/' ORDER BY path",
                    cols, prefix
                ),
                Vec::new(),
            ),
        };
        // `prepare_cached` on the *table's* connection: the cache lives as long as the
        // table, so the second and later reads through it compile nothing. There are only
        // ever a handful of distinct statements here — one per idx_num shape.
        let mut stmt = self.tab.conn().prepare_cached(&sql)?;
        let map = |r: &rusqlite::Row| -> rusqlite::Result<KbRow> {
            let root: Option<Vec<u8>> = r.get(4)?;
            Ok(KbRow {
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                kind: r.get(3)?,
                root: root.and_then(|v| v.try_into().ok()),
                version: r.get(5)?,
                nbytes: r.get(6)?,
                nlines: r.get(7)?,
                updated_at: r.get(8)?,
                updated_by: r.get(9)?,
            })
        };
        self.rows = stmt
            .query_map(rusqlite::params_from_iter(params), map)?
            .collect::<Result<Vec<_>>>()?;
        self.i = 0;
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.i += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.i >= self.rows.len()
    }

    fn column(&self, ctx: &mut Context, i: c_int) -> Result<()> {
        let r = &self.rows[self.i];
        match i {
            0 => ctx.set_result(&r.id),
            1 => ctx.set_result(&r.path),
            2 => ctx.set_result(&r.name),
            3 => ctx.set_result(&parent_of(&r.path)),
            4 => ctx.set_result(&if r.kind == 1 { "file" } else { "folder" }),
            5 => {
                if ctx.no_change() {
                    return Ok(());
                }
                match r.root {
                    Some(root) if r.kind == 1 => {
                        let st = crate::storage::SqliteStorage::new(self.tab.conn(), &self.tab.prefix);
                        let (bytes, utf8) = st.document(&root).map_err(map_err)?;
                        // The UTF-8 check already ran for exactly these bytes, and the
                        // root hash they are keyed by is derived from them, so the answer
                        // cannot belong to different content.
                        ctx.set_result(&if utf8 {
                            Value::Text(unsafe { String::from_utf8_unchecked(bytes.to_vec()) })
                        } else {
                            Value::Blob(bytes.to_vec())
                        })
                    }
                    _ => ctx.set_result(&Value::Null),
                }
            }
            6 => ctx.set_result(&r.version),
            7 => ctx.set_result(&r.nbytes),
            8 => ctx.set_result(&r.nlines),
            9 => ctx.set_result(&r.updated_at),
            10 => ctx.set_result(&r.updated_by),
            _ => ctx.set_result(&Value::Null),
        }
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.rows[self.i].id)
    }
}

pub const KB_MODULE: Module<KbTab> = Module::update_module();

// ---------------------------------------------------------------------------
// Table-valued functions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    Ls,
    Search,
    History,
    Export,
    Feed,
    Hunks,
    Chunks,
    PathHistory,
    PropKeys,
    PropValues,
    PropFind,
    Outline,
    Headings,
}

pub struct FnSpec {
    pub prefix: String,
    pub kind: FnKind,
}

impl FnKind {
    fn schema(self) -> &'static CStr {
        match self {
            FnKind::Ls => c"CREATE TABLE x(name TEXT, kind TEXT, nbytes INTEGER, nlines INTEGER, updated_at TEXT, path TEXT, nwords INTEGER, versions INTEGER, created_at TEXT, updated_by TEXT, nauthors INTEGER, authors TEXT, files INTEGER, folders INTEGER, id INTEGER, dir TEXT HIDDEN, recursive INTEGER HIDDEN)",
            FnKind::Search => c"CREATE TABLE x(path TEXT, line INTEGER, snippet TEXT, rank REAL, query TEXT HIDDEN, prefix TEXT HIDDEN, lim INTEGER HIDDEN)",
            FnKind::History => c"CREATE TABLE x(version INTEGER, author TEXT, ts TEXT, message TEXT, nbytes INTEGER, kind TEXT, base_version INTEGER, nlines INTEGER, nwords INTEGER, path TEXT HIDDEN)",
            FnKind::Export => c"CREATE TABLE x(path TEXT, content TEXT, prefix TEXT HIDDEN)",
            FnKind::Feed => c"CREATE TABLE x(seq INTEGER, ts TEXT, op TEXT, path TEXT, old_path TEXT, node_kind TEXT, version INTEGER, base_version INTEGER, commit_kind TEXT, author TEXT, message TEXT, since INTEGER HIDDEN, lim INTEGER HIDDEN)",
            FnKind::Hunks => c"CREATE TABLE x(old_from INTEGER, old_count INTEGER, new_from INTEGER, new_count INTEGER, old_text TEXT, new_text TEXT, path TEXT HIDDEN, v1 INTEGER HIDDEN, v2 INTEGER HIDDEN)",
            FnKind::Chunks => c"CREATE TABLE x(ord INTEGER, hash TEXT, byte_from INTEGER, nbytes INTEGER, line_from INTEGER, nlines INTEGER, path TEXT HIDDEN, version INTEGER HIDDEN)",
            FnKind::PropKeys => c"CREATE TABLE x(key TEXT, docs INTEGER, values_n INTEGER, kind TEXT, prefix TEXT HIDDEN, lim INTEGER HIDDEN)",
            FnKind::PropValues => c"CREATE TABLE x(value TEXT, docs INTEGER, key TEXT HIDDEN, prefix TEXT HIDDEN, lim INTEGER HIDDEN)",
            FnKind::PropFind => c"CREATE TABLE x(path TEXT, nbytes INTEGER, updated_at TEXT, frontmatter TEXT, query TEXT HIDDEN, folder TEXT HIDDEN, lim INTEGER HIDDEN)",
            FnKind::Outline => c"CREATE TABLE x(path TEXT, heading TEXT, heading_path TEXT, level INTEGER, line_from INTEGER, line_to INTEGER, nwords INTEGER, nwords_total INTEGER, nbytes INTEGER, nlines INTEGER, file_nwords INTEGER, version INTEGER, updated_at TEXT, updated_by TEXT, prefix TEXT HIDDEN, heading_match TEXT HIDDEN, mode TEXT HIDDEN, max_level INTEGER HIDDEN, lim INTEGER HIDDEN)",
            FnKind::Headings => c"CREATE TABLE x(heading TEXT, sections INTEGER, docs INTEGER, prefix TEXT HIDDEN, starts TEXT HIDDEN, lim INTEGER HIDDEN)",
            FnKind::PathHistory => c"CREATE TABLE x(id INTEGER, ts TEXT, op TEXT, old_path TEXT, new_path TEXT, via TEXT, version INTEGER, author TEXT, path TEXT HIDDEN, node_id INTEGER HIDDEN)",
        }
    }
    /// Number of visible columns; hidden argument columns follow.
    fn visible(self) -> c_int {
        match self {
            FnKind::Ls => 15,
            FnKind::Search => 4,
            FnKind::History => 9,
            FnKind::Export => 2,
            FnKind::Feed => 11,
            FnKind::Hunks => 6,
            FnKind::Chunks => 6,
            FnKind::PathHistory => 8,
            FnKind::PropKeys => 4,
            FnKind::PropValues => 2,
            FnKind::PropFind => 4,
            FnKind::Outline => 14,
            FnKind::Headings => 3,
        }
    }
    fn n_hidden(self) -> c_int {
        match self {
            FnKind::Ls => 2,
            FnKind::Search => 3,
            FnKind::History => 1,
            FnKind::Export => 1,
            FnKind::Feed => 2,
            FnKind::Hunks => 3,
            FnKind::Chunks => 2,
            FnKind::PathHistory => 2,
            FnKind::PropKeys => 2,
            FnKind::PropValues => 3,
            FnKind::PropFind => 3,
            FnKind::Outline => 5,
            FnKind::Headings => 3,
        }
    }
}

/// A hidden argument as an integer, accepting the text form a bound parameter can arrive in.
fn hidden_i64(v: &Option<Value>) -> Option<i64> {
    match v {
        Some(Value::Integer(i)) => Some(*i),
        Some(Value::Real(f)) => Some(*f as i64),
        Some(Value::Text(t)) => t.trim().parse().ok(),
        _ => None,
    }
}

#[repr(C)]
pub struct FnTab {
    base: ffi::sqlite3_vtab,
    db: *mut ffi::sqlite3,
    prefix: String,
    kind: FnKind,
    /// Same long-lived non-owning handle as `KbTab` holds, for the same reason: `db.rs` and
    /// `storage.rs` prepare everything through `prepare_cached`, and a `Connection` built
    /// per call throws that cache away before it can be used twice. `textdb_search` runs
    /// several statements per call, so it was recompiling all of them every time.
    conn: std::rc::Rc<Connection>,
}

impl Drop for FnTab {
    fn drop(&mut self) {
        crate::storage::release_shared_conn(self.db);
    }
}

unsafe impl<'vtab> VTab<'vtab> for FnTab {
    type Aux = FnSpec;
    type Cursor = FnCursor<'vtab>;

    fn connect(
        db: &mut VTabConnection,
        aux: Option<&FnSpec>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> Result<(Cow<'static, CStr>, Self)> {
        let spec = aux.ok_or_else(|| Error::ModuleError("missing function spec".into()))?;
        let handle = unsafe { db.handle() };
        let conn = unsafe { crate::storage::register_shared_conn(handle) }?;
        Ok((
            Cow::Borrowed(spec.kind.schema()),
            FnTab {
                base: ffi::sqlite3_vtab::default(),
                db: handle,
                prefix: spec.prefix.clone(),
                kind: spec.kind,
                conn,
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> Result<bool> {
        // Bitmask of hidden arguments present; argv indices assigned in column order.
        let vis = self.kind.visible();
        let mut present: Vec<(usize, c_int)> = Vec::new();
        for (i, c) in info.constraints().enumerate() {
            if c.is_usable() && c.operator() == IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ && c.column() >= vis {
                present.push((i, c.column() - vis));
            }
        }
        present.sort_by_key(|(_, col)| *col);
        let mut mask = 0;
        for (argv, (i, col)) in present.iter().enumerate() {
            let mut u = info.constraint_usage(*i);
            u.set_argv_index(argv as c_int + 1);
            u.set_omit(true);
            mask |= 1 << col;
        }
        info.set_idx_num(mask);
        info.set_estimated_cost(1000.0);
        Ok(true)
    }

    fn open(&'vtab mut self) -> Result<FnCursor<'vtab>> {
        Ok(FnCursor {
            base: ffi::sqlite3_vtab_cursor::default(),
            rows: Vec::new(),
            i: 0,
            kind: self.kind,
            tab: &*self,
        })
    }
}

impl FnTab {
    /// The table's own handle; see the field comment for why it lives here.
    fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl CreateVTab<'_> for FnTab {
    const KIND: VTabKind = VTabKind::EponymousOnly;
}

#[repr(C)]
pub struct FnCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    rows: Vec<Vec<Value>>,
    i: usize,
    kind: FnKind,
    tab: &'vtab FnTab,
}

unsafe impl VTabCursor for FnCursor<'_> {
    fn filter(&mut self, idx_num: c_int, _idx_str: Option<&str>, args: &Filters<'_>) -> Result<()> {
        // Decode hidden args from the bitmask.
        let mut hidden: Vec<Option<Value>> = vec![None; self.kind.n_hidden() as usize];
        let mut argv = 0;
        for col in 0..self.kind.n_hidden() {
            if idx_num & (1 << col) != 0 {
                hidden[col as usize] = Some(args.get::<Value>(argv)?);
                argv += 1;
            }
        }
        let s = |v: &Option<Value>| -> Option<String> {
            match v {
                Some(Value::Text(t)) => Some(t.clone()),
                Some(Value::Blob(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Some(Value::Integer(i)) => Some(i.to_string()),
                _ => None,
            }
        };
        let db = TextDb::attach(self.tab.conn(), &self.tab.prefix, false);
        self.rows = match self.kind {
            FnKind::Ls => {
                let dir = s(&hidden[0]).unwrap_or_else(|| "/".into());
                let recursive = hidden_i64(&hidden[1]).unwrap_or(0) != 0;
                let int = |v: Option<i64>| v.map_or(Value::Null, Value::Integer);
                db.list(&dir, recursive)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|e| {
                        let file = e.kind == 1;
                        let authors: Vec<_> = e.authors.iter().map(crate::db::AuthorCount::to_json).collect();
                        vec![
                            Value::Text(e.name),
                            Value::Text(if file { "file".into() } else { "folder".into() }),
                            int(e.nbytes),
                            int(e.nlines),
                            Value::Text(e.updated_at),
                            Value::Text(e.path),
                            int(e.nwords),
                            Value::Integer(e.versions),
                            Value::Text(e.created_at),
                            e.updated_by.map_or(Value::Null, Value::Text),
                            if file { Value::Integer(authors.len() as i64) } else { Value::Null },
                            Value::Text(serde_json::Value::Array(authors).to_string()),
                            int(e.files),
                            int(e.folders),
                            Value::Integer(e.id),
                        ]
                    })
                    .collect()
            }
            FnKind::Search => {
                let q = s(&hidden[0]).unwrap_or_default();
                let prefix = s(&hidden[1]).unwrap_or_else(|| "/".into());
                let lim = match hidden_i64(&hidden[2]) {
                    Some(i) => i.max(0) as usize,
                    _ => 100,
                };
                db.search(&q, &prefix, lim)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|h| vec![Value::Text(h.path), Value::Integer(h.line), Value::Text(h.snippet), Value::Real(h.rank)])
                    .collect()
            }
            FnKind::Outline => {
                let prefix = s(&hidden[0]).unwrap_or_else(|| "/".into());
                let heading = s(&hidden[1]);
                let mode = crate::sections::Match::parse(&s(&hidden[2]).unwrap_or_default());
                let max_level = hidden_i64(&hidden[3]);
                let lim = hidden_i64(&hidden[4]).map_or(1000, |i| i.max(0) as usize);
                let int = |v: Option<i64>| v.map_or(Value::Null, Value::Integer);
                crate::sections::outline(self.tab.conn(), &self.tab.prefix, &prefix, heading.as_deref(), mode, max_level, lim)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|r| {
                        vec![
                            Value::Text(r.path),
                            Value::Text(r.heading),
                            Value::Text(r.heading_path),
                            Value::Integer(r.level),
                            Value::Integer(r.line_from),
                            Value::Integer(r.line_to),
                            int(r.nwords),
                            int(r.nwords_total),
                            int(r.file_nbytes),
                            int(r.file_nlines),
                            int(r.file_nwords),
                            Value::Integer(r.file_version),
                            Value::Text(r.updated_at),
                            r.updated_by.map_or(Value::Null, Value::Text),
                        ]
                    })
                    .collect()
            }
            FnKind::Headings => {
                let prefix = s(&hidden[0]).unwrap_or_else(|| "/".into());
                let starts = s(&hidden[1]).unwrap_or_default();
                let lim = hidden_i64(&hidden[2]).map_or(100, |i| i.max(0) as usize);
                crate::sections::heading_names(self.tab.conn(), &self.tab.prefix, &prefix, &starts, lim)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|(h, n, d)| vec![Value::Text(h), Value::Integer(n), Value::Integer(d)])
                    .collect()
            }
            FnKind::PropKeys => {
                let prefix = s(&hidden[0]).unwrap_or_default();
                let lim = hidden_i64(&hidden[1]).map(|i| i.max(0) as usize).unwrap_or(200);
                db.property_keys(&prefix, lim)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|k| {
                        vec![
                            Value::Text(k.key),
                            Value::Integer(k.docs),
                            Value::Integer(k.values),
                            Value::Text(k.kind),
                        ]
                    })
                    .collect()
            }
            FnKind::PropValues => {
                let key = s(&hidden[0]).ok_or_else(|| Error::ModuleError("TX004 key is required".into()))?;
                let prefix = s(&hidden[1]).unwrap_or_default();
                let lim = hidden_i64(&hidden[2]).map(|i| i.max(0) as usize).unwrap_or(200);
                db.property_values(&key, &prefix, lim)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|v| vec![v.value.map(Value::Text).unwrap_or(Value::Null), Value::Integer(v.docs)])
                    .collect()
            }
            FnKind::PropFind => {
                let q = s(&hidden[0]).unwrap_or_default();
                let folder = s(&hidden[1]).unwrap_or_else(|| "/".into());
                let lim = hidden_i64(&hidden[2]).map(|i| i.max(0) as usize).unwrap_or(500);
                db.property_find(&q, &folder, lim)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|h| {
                        vec![
                            Value::Text(h.path),
                            Value::Integer(h.nbytes),
                            Value::Text(h.updated_at),
                            h.frontmatter.map(Value::Text).unwrap_or(Value::Null),
                        ]
                    })
                    .collect()
            }
            FnKind::History => {
                let path = s(&hidden[0]).ok_or_else(|| Error::ModuleError("TX004 path is required".into()))?;
                db.history(&path)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|c| {
                        vec![
                            Value::Integer(c.version),
                            c.author.map_or(Value::Null, Value::Text),
                            Value::Text(c.ts),
                            c.message.map_or(Value::Null, Value::Text),
                            c.nbytes.map_or(Value::Null, Value::Integer),
                            c.kind.map_or(Value::Null, Value::Text),
                            c.base_version.map_or(Value::Null, Value::Integer),
                            c.nlines.map_or(Value::Null, Value::Integer),
                            c.nwords.map_or(Value::Null, Value::Integer),
                        ]
                    })
                    .collect()
            }
            FnKind::Feed => {
                let since = hidden_i64(&hidden[0]).unwrap_or(0);
                let lim = hidden_i64(&hidden[1]).map_or(10_000, |v| v.max(0) as usize);
                db.feed(since, lim)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|c| {
                        vec![
                            Value::Integer(c.seq),
                            Value::Text(c.ts),
                            Value::Text(c.op),
                            Value::Text(c.path),
                            c.old_path.map_or(Value::Null, Value::Text),
                            Value::Text(if c.node_kind == 1 { "file".into() } else { "folder".into() }),
                            c.version.map_or(Value::Null, Value::Integer),
                            c.base_version.map_or(Value::Null, Value::Integer),
                            c.commit_kind.map_or(Value::Null, Value::Text),
                            c.author.map_or(Value::Null, Value::Text),
                            c.message.map_or(Value::Null, Value::Text),
                        ]
                    })
                    .collect()
            }
            FnKind::Hunks => {
                let path = s(&hidden[0]).ok_or_else(|| Error::ModuleError("TX004 path is required".into()))?;
                // `textdb_hunks(path)` is the latest commit and `textdb_hunks(path, v1)` runs
                // from `v1` to HEAD, so a client that knows its version needs no other lookup.
                let (v1, v2) = match (hidden_i64(&hidden[1]), hidden_i64(&hidden[2])) {
                    (Some(a), Some(b)) => (a, b),
                    (a, b) => {
                        let head = db
                            .node_by_path_any(&normalize_path(&path).map_err(map_err)?)
                            .map_err(map_err)?
                            .ok_or_else(|| map_err(TextdbError::NotFound(path.clone())))?
                            .version;
                        let b = b.unwrap_or(head);
                        (a.unwrap_or(b - 1), b)
                    }
                };
                // Line numbers are 1-based here, as in `textdb_lines`.
                db.hunks(&path, v1.max(0) as u64, v2.max(0) as u64)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|h| {
                        vec![
                            Value::Integer(h.old_from as i64 + 1),
                            Value::Integer(h.old_count as i64),
                            Value::Integer(h.new_from as i64 + 1),
                            Value::Integer(h.new_count as i64),
                            bytes_value(h.old_text),
                            bytes_value(h.new_text),
                        ]
                    })
                    .collect()
            }
            FnKind::Chunks => {
                let path = s(&hidden[0]).ok_or_else(|| Error::ModuleError("TX004 path is required".into()))?;
                let version = hidden_i64(&hidden[1]).map(|v| v.max(0) as u64);
                db.chunks(&path, version)
                    .map_err(map_err)?
                    .into_iter()
                    .enumerate()
                    .map(|(i, l)| {
                        vec![
                            Value::Integer(i as i64),
                            Value::Text(textdb_core::hash::hex(&l.hash)),
                            Value::Integer(l.byte_off as i64),
                            Value::Integer(l.nbytes as i64),
                            Value::Integer(l.line_off as i64 + 1),
                            Value::Integer(l.nlines as i64),
                        ]
                    })
                    .collect()
            }
            FnKind::PathHistory => {
                // `textdb_path_history(path)`, or `textdb_path_history(NULL, node_id)` for a
                // node no path names any more (a trash entry).
                let events = match hidden_i64(&hidden[1]) {
                    Some(id) => db.path_history_of(id),
                    None => {
                        let path = s(&hidden[0]).ok_or_else(|| Error::ModuleError("TX004 path is required".into()))?;
                        db.path_history(&path)
                    }
                }
                .map_err(map_err)?;
                events
                    .into_iter()
                    .map(|e| {
                        vec![
                            Value::Integer(e.id),
                            Value::Text(e.ts),
                            Value::Text(e.op),
                            Value::Text(e.old_path),
                            e.new_path.map_or(Value::Null, Value::Text),
                            e.via.map_or(Value::Null, Value::Text),
                            e.version.map_or(Value::Null, Value::Integer),
                            e.author.map_or(Value::Null, Value::Text),
                        ]
                    })
                    .collect()
            }
            FnKind::Export => {
                let prefix = s(&hidden[0]).unwrap_or_else(|| "/".into());
                db.export(&prefix)
                    .map_err(map_err)?
                    .into_iter()
                    .map(|(p, c)| vec![Value::Text(p), bytes_value(c)])
                    .collect()
            }
        };
        self.i = 0;
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.i += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.i >= self.rows.len()
    }

    fn column(&self, ctx: &mut Context, i: c_int) -> Result<()> {
        match self.rows[self.i].get(i as usize) {
            Some(v) => ctx.set_result(v),
            None => ctx.set_result(&Value::Null),
        }
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.i as i64)
    }
}

pub const FN_MODULE: Module<FnTab> = Module::eponymous_only_module();
