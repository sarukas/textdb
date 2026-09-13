import { existsSync } from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';
import { TextdbError, toTextdbError } from './errors.ts';

const LIBRARY_NAMES = ['textdb_sqlite_ext.dll', 'libtextdb_sqlite_ext.so', 'libtextdb_sqlite_ext.dylib'];

// node/packages/textdb/src → repository root.
const REPO_BUILD_DIR = fileURLToPath(new URL('../../../../crates/textdb-sqlite-ext/target/release/', import.meta.url));

/** The loadable extension: the explicit option, then `TEXTDB_SQLITE_EXT`, then the repository's release build. */
export function resolveExtension(explicit?: string): string {
  const configured = explicit || process.env.TEXTDB_SQLITE_EXT;
  if (configured) return configured;
  for (const name of LIBRARY_NAMES) {
    const candidate = path.join(REPO_BUILD_DIR, name);
    if (existsSync(candidate)) return candidate;
  }
  throw new TextdbError(
    `textdb SQLite extension not found in ${REPO_BUILD_DIR}. ` +
      'Build it with `cargo build --release` in crates/textdb-sqlite-ext, or set TEXTDB_SQLITE_EXT.',
  );
}

/** A connection with the extension loaded, WAL on and the `kb` table declared. */
export function openConnection(dbPath: string, extension: string): DatabaseSync {
  const conn = new DatabaseSync(dbPath, { allowExtension: true });
  try {
    conn.loadExtension(extension);
    conn.enableLoadExtension(false);
    conn.exec('PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=30000');
    conn.exec("CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_')");
  } catch (error) {
    conn.close();
    throw toTextdbError(error);
  }
  return conn;
}
