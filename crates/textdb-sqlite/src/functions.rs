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

fn with_db<T>(ctx: &Context, prefix: &str, f: impl FnOnce(&TextDb) -> textdb_core::storage::Result<T>) -> Result<T> {
    let conn = unsafe { ctx.get_connection()? };
    // Scalar functions run inside a SELECT: open one write transaction for the whole
    // operation instead of autocommitting every nested statement.
    let db = TextDb::attach(&conn, prefix, true);
    f(&db).map_err(map_err)
}

pub fn register_functions(conn: &Connection, prefix: &str) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_content", -1, flags, move |ctx| {
        if ctx.len() < 1 {
            return Err(Error::UserFunctionError("textdb_content(path[, version])".into()));
        }
        let path = arg_str(ctx, 0)?;
        let bytes = if ctx.len() >= 2 && ctx.get_raw(1) != ValueRef::Null {
            let v: i64 = ctx.get(1)?;
            with_db(ctx, &p, |db| db.read_version(&path, v as u64))?
        } else {
            with_db(ctx, &p, |db| db.read(&path))?
        };
        Ok(text_or_blob(bytes))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_lines", 3, flags, move |ctx| {
        let path = arg_str(ctx, 0)?;
        let from: i64 = ctx.get(1)?;
        let to: i64 = ctx.get(2)?;
        let bytes = with_db(ctx, &p, |db| db.lines(&path, from.max(0) as u64, to.max(0) as u64))?;
        Ok(text_or_blob(bytes))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_section", 2, flags, move |ctx| {
        let path = arg_str(ctx, 0)?;
        let heading = arg_str(ctx, 1)?;
        let bytes = with_db(ctx, &p, |db| db.section(&path, &heading))?;
        Ok(bytes.map_or(Value::Null, text_or_blob))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_diff", 3, flags, move |ctx| {
        let path = arg_str(ctx, 0)?;
        let v1: i64 = ctx.get(1)?;
        let v2: i64 = ctx.get(2)?;
        with_db(ctx, &p, |db| db.diff(&path, v1 as u64, v2 as u64))
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_edit", -1, flags, move |ctx| {
        if ctx.len() < 3 {
            return Err(Error::UserFunctionError("textdb_edit(path, old, new[, author])".into()));
        }
        let path = arg_str(ctx, 0)?;
        let old = arg_bytes(ctx, 1)?;
        let new = arg_bytes(ctx, 2)?;
        let author = if ctx.len() >= 4 { Some(arg_str(ctx, 3)?) } else { None };
        let r = with_db(ctx, &p, |db| db.edit(&path, &old, &new, author.as_deref()))?;
        Ok(r.version as i64)
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_append", -1, flags, move |ctx| {
        if ctx.len() < 2 {
            return Err(Error::UserFunctionError("textdb_append(path, text[, author])".into()));
        }
        let path = arg_str(ctx, 0)?;
        let tail = arg_bytes(ctx, 1)?;
        let author = if ctx.len() >= 3 { Some(arg_str(ctx, 2)?) } else { None };
        let r = with_db(ctx, &p, |db| db.append(&path, &tail, author.as_deref()))?;
        Ok(r.version as i64)
    })?;
    let p = prefix.to_string();
    conn.create_scalar_function("textdb_checkpoint", 1, flags, move |ctx| {
        let name = arg_str(ctx, 0)?;
        let n = with_db(ctx, &p, |db| db.checkpoint(&name))?;
        Ok(n as i64)
    })?;
    Ok(())
}
