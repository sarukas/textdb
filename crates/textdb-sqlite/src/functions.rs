//! Scalar SQL functions bound to a store prefix (spec §7.1): `textdb_content(path[, version])`,
//! `textdb_lines(path, from, to)`, `textdb_section(path, heading)`, `textdb_diff(path, v1, v2)`,
//! `textdb_edit(path, old, new)`, `textdb_append(path, text)`, `textdb_checkpoint(name)`.

use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, Error, Result};

use crate::db::TextDb;
use crate::vtab::map_err;

fn arg_bytes(ctx: &Context, i: usize) -> Result<Vec<u8>> {
    Ok(match ctx.get_raw(i) {
        ValueRef::Text(t) => t.to_vec(),
        ValueRef::Blob(b) => b.to_vec(),
        ValueRef::Integer(n) => n.to_string().into_bytes(),
        ValueRef::Real(f) => f.to_string().into_bytes(),
        ValueRef::Null => Vec::new(),
    })
}

fn arg_str(ctx: &Context, i: usize) -> Result<String> {
    Ok(String::from_utf8_lossy(&arg_bytes(ctx, i)?).into_owned())
}

fn text_or_blob(b: Vec<u8>) -> Value {
    match String::from_utf8(b) {
        Ok(s) => Value::Text(s),
        Err(e) => Value::Blob(e.into_bytes()),
    }
}

/// As `text_or_blob`, for bytes that arrive with their UTF-8 answer already known.
///
/// `document()` validates once and caches the result next to the bytes, because both are
/// properties of content keyed by its own hash. Re-deriving it with `String::from_utf8`
/// walks the whole document again on every call — a second pass over a megabyte to learn
/// something already recorded. `from_utf8_unchecked` is sound here for the same reason the
/// virtual table's content column relies on: the flag was computed from exactly these
/// bytes, and a cache entry cannot hold a flag from different content.
fn text_or_blob_checked(b: &[u8], utf8: bool) -> Value {
    if utf8 {
        Value::Text(unsafe { String::from_utf8_unchecked(b.to_vec()) })
    } else {
        Value::Blob(b.to_vec())
    }
}

/// Argument `i` as an integer, `None` when absent or NULL.
fn opt_i64(ctx: &Context, i: usize) -> Result<Option<i64>> {
    if i >= ctx.len() {
        return Ok(None);
    }
    Ok(match ctx.get_raw(i) {
        ValueRef::Integer(n) => Some(n),
        // Some drivers bind every number as a double (Node's node:sqlite does), so a
        // whole-valued REAL is an integer here; a fractional one is not.
        ValueRef::Real(f) if f.fract() == 0.0 => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|s| s.trim().parse().ok()),
        ValueRef::Real(_) | ValueRef::Null | ValueRef::Blob(_) => None,
    })
}

/// Argument `i` as an integer, required.
fn arg_i64(ctx: &Context, i: usize) -> Result<i64> {
    opt_i64(ctx, i)?.ok_or_else(|| Error::UserFunctionError(format!("argument {} must be an integer", i + 1).into()))
}

/// Argument `i` as text, `None` when absent or NULL.
fn opt_str(ctx: &Context, i: usize) -> Result<Option<String>> {
    if i >= ctx.len() || ctx.get_raw(i) == ValueRef::Null {
        return Ok(None);
    }
    arg_str(ctx, i).map(Some)
}

/// A write's outcome as JSON, `{"version":3,"kind":"rebased"}`. A caller showing how its
/// commit landed needs the kind as well as the version, and a scalar returns one value.
fn write_json(r: &crate::db::WriteResult) -> String {
    serde_json::json!({ "version": r.version, "kind": r.kind.as_str() }).to_string()
}

/// Run `f` against the store, on the best handle available.
///
/// Prefer the handle a `textdb` virtual table registered on this connection: everything
/// `TextDb` runs goes through `prepare_cached`, and a handle created per call throws that
/// cache away before it can be used twice — most of what a scalar function costs over the
/// same read through the table. `storage::shared` explains why the table's handle may hold
/// prepared statements where a function closure may not. With no table registered this falls
/// back to a per-call handle and is merely slower.
///
/// `handle` is the connection these functions were registered on, captured as a `usize`
/// because a raw pointer is not `Send` and the closures must be. It is valid for every call:
/// SQLite only invokes a function while the connection that owns it is open, and closing
/// that connection unregisters it.
fn with_db<T>(
    ctx: &Context,
    handle: usize,
    prefix: &str,
    f: impl FnOnce(&TextDb) -> textdb_core::storage::Result<T>,
) -> Result<T> {
    let shared = crate::storage::shared_conn(handle as *mut rusqlite::ffi::sqlite3);
    let owned;
    let conn: &rusqlite::Connection = match &shared {
        Some(c) => c,
        None => {
            owned = unsafe { ctx.get_connection()? };
            &owned
        }
    };
    // Scalar functions run inside a SELECT: open one write transaction for the whole
    // operation instead of autocommitting every nested statement.
    let db = TextDb::attach(conn, prefix, true);
    f(&db).map_err(map_err)
}

pub fn register_functions(conn: &Connection, prefix: &str) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY;
    // See `with_db`: the handle these functions belong to, captured once.
    let h = unsafe { conn.handle() } as usize;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_content", -1, flags, move |ctx| {
        if ctx.len() < 1 {
            return Err(Error::UserFunctionError("textdb_content(path[, version])".into()));
        }
        let path = arg_str(ctx, 0)?;
        let (bytes, utf8) = if ctx.len() >= 2 && ctx.get_raw(1) != ValueRef::Null {
            let v = arg_i64(ctx, 1)?;
            with_db(ctx, h, &p, |db| db.read_version_shared(&path, v as u64))?
        } else {
            with_db(ctx, h, &p, |db| db.read_shared(&path))?
        };
        Ok(text_or_blob_checked(&bytes, utf8))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_lines", 3, flags, move |ctx| {
        let path = arg_str(ctx, 0)?;
        let from = arg_i64(ctx, 1)?;
        let to = arg_i64(ctx, 2)?;
        let bytes = with_db(ctx, h, &p, |db| db.lines(&path, from.max(0) as u64, to.max(0) as u64))?;
        Ok(text_or_blob(bytes))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_section", 2, flags, move |ctx| {
        let path = arg_str(ctx, 0)?;
        let heading = arg_str(ctx, 1)?;
        let bytes = with_db(ctx, h, &p, |db| db.section(&path, &heading))?;
        Ok(bytes.map_or(Value::Null, text_or_blob))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_diff", 3, flags, move |ctx| {
        let path = arg_str(ctx, 0)?;
        let v1 = arg_i64(ctx, 1)?;
        let v2 = arg_i64(ctx, 2)?;
        with_db(ctx, h, &p, |db| db.diff(&path, v1 as u64, v2 as u64))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_edit", -1, flags, move |ctx| {
        if ctx.len() < 3 {
            return Err(Error::UserFunctionError("textdb_edit(path, old, new[, author])".into()));
        }
        let path = arg_str(ctx, 0)?;
        let old = arg_bytes(ctx, 1)?;
        let new = arg_bytes(ctx, 2)?;
        // NULL or '' both mean "no author", so callers that always pass the argument do not
        // record an empty name.
        let author = opt_str(ctx, 3)?.filter(|a| !a.is_empty());
        let r = with_db(ctx, h, &p, |db| db.edit(&path, &old, &new, author.as_deref()))?;
        Ok(r.version as i64)
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_append", -1, flags, move |ctx| {
        if ctx.len() < 2 {
            return Err(Error::UserFunctionError("textdb_append(path, text[, author])".into()));
        }
        let path = arg_str(ctx, 0)?;
        let tail = arg_bytes(ctx, 1)?;
        let author = opt_str(ctx, 2)?.filter(|a| !a.is_empty());
        let r = with_db(ctx, h, &p, |db| db.append(&path, &tail, author.as_deref()))?;
        Ok(r.version as i64)
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_checkpoint", 1, flags, move |ctx| {
        let name = arg_str(ctx, 0)?;
        let n = with_db(ctx, h, &p, |db| db.checkpoint(&name))?;
        Ok(n as i64)
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_write", -1, flags, move |ctx| {
        if ctx.len() < 2 {
            return Err(Error::UserFunctionError(
                "textdb_write(path, content[, base_version[, author[, message]]])".into(),
            ));
        }
        let path = arg_str(ctx, 0)?;
        let content = arg_bytes(ctx, 1)?;
        let base = opt_i64(ctx, 2)?.map(|v| v.max(0) as u64);
        let author = opt_str(ctx, 3)?;
        let message = opt_str(ctx, 4)?;
        let r = with_db(ctx, h, &p, |db| db.write(&path, &content, base, author.as_deref(), message.as_deref()))?;
        Ok(write_json(&r))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_replace_lines", -1, flags, move |ctx| {
        if ctx.len() < 4 {
            return Err(Error::UserFunctionError(
                "textdb_replace_lines(path, from, to, text[, base_version[, author]])".into(),
            ));
        }
        let path = arg_str(ctx, 0)?;
        let from = arg_i64(ctx, 1)?;
        let to = arg_i64(ctx, 2)?;
        let text = arg_bytes(ctx, 3)?;
        let base = opt_i64(ctx, 4)?.map(|v| v.max(0) as u64);
        let author = opt_str(ctx, 5)?;
        let r = with_db(ctx, h, &p, |db| {
            db.replace_lines(&path, from.max(0) as u64, to.max(0) as u64, &text, base, author.as_deref())
        })?;
        Ok(write_json(&r))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_last_seq", 0, flags, move |ctx| with_db(ctx, h, &p, |db| db.last_seq()))?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_migrate", 0, flags, move |ctx| {
        let added = with_db(ctx, h, &p, |db| crate::schema::migrate(db.conn, &db.p).map_err(crate::storage::sql_err))?;
        Ok(added as i64)
    })?;
    Ok(())
}
