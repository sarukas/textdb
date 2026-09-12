//! Loadable SQLite extension: `.load libtextdb_sqlite_ext` (sqlite3 shell) or
//! `conn.load_extension("libtextdb_sqlite_ext")` (Python `sqlite3`, any language).
//!
//! Registers the `textdb` virtual-table module, the table-valued functions and the scalar
//! functions bound to store prefix `kb_`. Entry points: `sqlite3_textdbsqliteext_init`
//! (derived from the library name) and the generic `sqlite3_extension_init`.

use std::os::raw::{c_char, c_int};

use rusqlite::ffi;
use rusqlite::Connection;

fn init(db: *mut ffi::sqlite3, p_api: *mut ffi::sqlite3_api_routines) -> rusqlite::Result<()> {
    let conn = unsafe { Connection::extension_init2(db, p_api)? };
    textdb_sqlite::register(&conn, textdb_sqlite::DEFAULT_PREFIX)
}

/// # Safety
/// Called by SQLite with valid pointers.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_textdbsqliteext_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    match init(db, p_api) {
        Ok(()) => ffi::SQLITE_OK,
        Err(e) => {
            if !pz_err_msg.is_null() {
                let msg = std::ffi::CString::new(e.to_string()).unwrap_or_default();
                *pz_err_msg = ffi::sqlite3_mprintf(msg.as_ptr());
            }
            ffi::SQLITE_ERROR
        }
    }
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
