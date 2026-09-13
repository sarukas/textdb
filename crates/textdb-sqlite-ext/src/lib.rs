//! Loadable SQLite extension: `.load libtextdb_sqlite_ext` (sqlite3 shell) or
//! `conn.load_extension("libtextdb_sqlite_ext")` (Python `sqlite3`, any language).
//!
//! Registers the `textdb` virtual-table module, the table-valued functions and the scalar
//! functions bound to store prefix `kb_`. Entry points: `sqlite3_textdbsqliteext_init`
//! (derived from the library name) and the generic `sqlite3_extension_init`.

use std::os::raw::{c_char, c_int};

use rusqlite::ffi;
use rusqlite::Connection;

/// Register everything on the connection SQLite is loading us into.
///
/// `Ok(false)` rather than `Ok(true)`: everything here is registered per connection, so
/// the init routine has to run again for the next one. `Ok(true)` would answer
/// `SQLITE_OK_LOAD_PERMANENTLY`, which tells SQLite to keep the library resident and *not*
/// re-run init — correct only for an extension that installs process-global state.
fn init(conn: Connection) -> rusqlite::Result<bool> {
    textdb_sqlite::register(&conn, textdb_sqlite::DEFAULT_PREFIX)?;
    Ok(false)
}

/// # Safety
/// Called by SQLite with valid pointers.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_textdbsqliteext_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    // `extension_init2` binds the API routine table, wraps `db` in a non-owning
    // `Connection` (so dropping it does not close the caller's database), writes any error
    // into `pz_err_msg` with SQLite's own allocator and returns the right SQLITE_ code.
    Connection::extension_init2(db, pz_err_msg, p_api, init)
}

/// # Safety
/// Called by SQLite with valid pointers.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_extension_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    sqlite3_textdbsqliteext_init(db, pz_err_msg, p_api)
}
