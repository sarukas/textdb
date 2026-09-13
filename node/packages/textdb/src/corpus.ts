import path from 'node:path';
import { openConnection, resolveExtension } from './connection.ts';
import { NotFound, TextdbError } from './errors.ts';
import { Sql, asText, placeholders } from './sql.ts';
import type {
  Change,
  Chunk,
  Entry,
  FileView,
  HistoryEntry,
  Hunk,
  Info,
  SearchHit,
  WriteResult,
} from './types.ts';
import { type WatchOptions, Watcher } from './watch.ts';

export interface OpenOptions {
  /** Path of the SQLite store. */
  db: string;
  /** Path of the loadable extension; see `resolveExtension`. */
  extension?: string;
  /** Default author for writes that do not name one. */
  author?: string;
}

export interface WriteOptions {
  baseVersion?: number;
  author?: string;
  message?: string;
}

export interface AuthorOptions {
  author?: string;
}

export interface ImportFile {
  path: string;
  content: string;
}

export interface ImportFailure {
  path: string;
  code: string;
  message: string;
}

export interface ImportStats {
  created: number;
  updated: number;
  unchanged: number;
  failed: number;
  failures: ImportFailure[];
}

/** Something a delete left behind, readable until it is purged. */
export interface TrashEntry {
  id: number;
  name: string;
  kind: 'file' | 'folder';
  /** Where it was when it was deleted. */
  path: string;
  version: number;
  /** A file's size; for a folder, the total of the files deleted with it. */
  nbytes: number;
  nlines: number | null;
  /** 1 for a file; for a folder, the files deleted with it. */
  files: number;
  updated_at: string;
  updated_by: string | null;
  deleted_at: string;
  deleted_by: string | null;
}

export interface PurgeStats {
  /** Trash items removed whole. */
  items: number;
  files: number;
  folders: number;
  versions: number;
  chunks: number;
  tree_nodes: number;
  /** Content bytes freed: what nothing remaining shares. */
  bytes: number;
}

export interface Stat {
  path: string;
  kind: 'file' | 'folder';
  /** Files in the subtree (1 for a file). */
  files: number;
  /** Folders below a folder, not counting itself. */
  folders: number;
  nbytes: number;
}

export function openCorpus(options: OpenOptions): Corpus {
  const extension = resolveExtension(options.extension);
  const db = options.db === ':memory:' ? options.db : path.resolve(options.db);
  const sql = new Sql(openConnection(db, extension));
  try {
    // A store written by an older build lacks the change feed and commit columns.
    sql.value('SELECT textdb_migrate()');
  } catch (error) {
    sql.conn.close();
    throw error;
  }
  return new Corpus(sql, db, extension, options.author ?? null);
}

interface HeadRow {
  version: number;
  content: unknown;
  nbytes: number;
  nlines: number;
  updated_at: string;
  updated_by: string | null;
}

export class Corpus {
  readonly db: string;
  readonly extension: string;
  readonly author: string | null;
  private readonly sql: Sql;

  constructor(sql: Sql, db: string, extension: string, author: string | null) {
    this.sql = sql;
    this.db = db;
    this.extension = extension;
    this.author = author;
  }

  info(): Info {
    const files = Number(this.sql.value("SELECT count(*) FROM kb WHERE kind = 'file'"));
    return { db: this.db, files, last_seq: this.lastSeq() };
  }

  /** One folder level: folders first, then files, each in the store's name order. */
  ls(dir = '/'): Entry[] {
    const rows = this.sql.all<Entry>('SELECT name, path, kind, nbytes, nlines, updated_at FROM textdb_ls(?)', dir);
    return rows.sort((a, b) => Number(a.kind === 'file') - Number(b.kind === 'file'));
  }

  read(filePath: string, version?: number): FileView {
    const head = this.sql.get<HeadRow>(
      "SELECT version, content, nbytes, nlines, updated_at, updated_by FROM kb WHERE path = ? AND kind = 'file'",
      filePath,
    );
    if (!head) throw new NotFound(`not found: ${filePath}`);
    if (version === undefined || version === head.version) {
      const { content, ...rest } = head;
      return { path: filePath, head_version: head.version, content: asText(content), ...rest };
    }
    const content = asText(this.sql.value('SELECT textdb_content(?, ?)', filePath, version));
    const commit = this.sql.get<{ author: string | null; ts: string; nbytes: number }>(
      'SELECT author, ts, nbytes FROM textdb_history(?) WHERE version = ?',
      filePath,
      version,
    );
    if (!commit) throw new NotFound(`not found: version ${version} of ${filePath}`);
    return {
      path: filePath,
      version,
      head_version: head.version,
      content,
      nbytes: commit.nbytes,
      nlines: countNewlines(content),
      updated_at: commit.ts,
      updated_by: commit.author,
    };
  }

  chunks(filePath: string, version?: number): Chunk[] {
    const args = version === undefined ? [filePath] : [filePath, version];
    return this.sql.all<Chunk>(
      `SELECT ord, hash, byte_from, nbytes, line_from, nlines FROM textdb_chunks(${placeholders(args)})`,
      ...args,
    );
  }

  /** Oldest first. */
  history(filePath: string): HistoryEntry[] {
    return this.sql.all<HistoryEntry>(
      'SELECT version, author, ts, message, nbytes, kind, base_version FROM textdb_history(?)',
      filePath,
    );
  }

  /** Line hunks turning `from` into `to`; `to` defaults to HEAD and `from` to `to - 1`. */
  hunks(filePath: string, from?: number, to?: number): Hunk[] {
    const args: (string | number)[] = [filePath];
    if (from !== undefined || to !== undefined) args.push(from ?? (to as number) - 1);
    if (to !== undefined) args.push(to);
    return this.sql.all<Hunk>(
      `SELECT old_from, old_count, new_from, new_count, old_text, new_text FROM textdb_hunks(${placeholders(args)})`,
      ...args,
    );
  }

  diff(filePath: string, from: number, to: number): string {
    return asText(this.sql.value('SELECT textdb_diff(?, ?, ?)', filePath, from, to));
  }

  search(query: string, options: { prefix?: string; limit?: number } = {}): SearchHit[] {
    return this.sql.all<SearchHit>(
      'SELECT path, line, snippet, rank FROM textdb_search(?, ?, ?)',
      query,
      options.prefix ?? '/',
      options.limit ?? 50,
    );
  }

  /** Writes the whole document, creating it if missing; rebased over commits newer than `baseVersion`. */
  write(filePath: string, content: string, options: WriteOptions = {}): WriteResult {
    return parseWrite(
      this.sql.value(
        'SELECT textdb_write(?, ?, ?, ?, ?)',
        filePath,
        content,
        options.baseVersion ?? null,
        this.authorOf(options),
        options.message ?? null,
      ),
    );
  }

  /** Replaces lines `from..to` (1-based, inclusive) of `baseVersion`; `to = from - 1` inserts. */
  replaceLines(filePath: string, from: number, to: number, text: string, options: Omit<WriteOptions, 'message'> = {}): WriteResult {
    return parseWrite(
      this.sql.value(
        'SELECT textdb_replace_lines(?, ?, ?, ?, ?, ?)',
        filePath,
        from,
        to,
        text,
        options.baseVersion ?? null,
        this.authorOf(options),
      ),
    );
  }

  /** Replaces the unique occurrence of `oldText`; returns the new version. */
  edit(filePath: string, oldText: string, newText: string, options: AuthorOptions = {}): number {
    // textdb_edit reads a NULL author as '', so the argument is left out instead.
    const args = [filePath, oldText, newText, ...this.optionalAuthor(options)];
    return Number(this.sql.value(`SELECT textdb_edit(${placeholders(args)})`, ...args));
  }

  append(filePath: string, text: string, options: AuthorOptions = {}): number {
    const args = [filePath, text, ...this.optionalAuthor(options)];
    return Number(this.sql.value(`SELECT textdb_append(${placeholders(args)})`, ...args));
  }

  /** Moves a file or a whole folder. */
  /** Moves or renames a file, or a folder with everything below it. */
  move(from: string, to: string, options: AuthorOptions = {}): void {
    this.sql.value('SELECT textdb_move(?, ?, ?)', from, to, this.authorOf(options));
  }

  /** Deletes a file, or a folder with everything below it. History stays in the store. */
  remove(target: string, options: AuthorOptions = {}): void {
    this.sql.value('SELECT textdb_delete(?, ?)', target, this.authorOf(options));
  }

  /** Trash items, newest delete first; with `parent`, what was deleted inside that trashed folder. */
  trash(parent?: number): TrashEntry[] {
    return JSON.parse(String(this.sql.value('SELECT textdb_trash(?)', parent ?? null))) as TrashEntry[];
  }

  trashEntry(id: number): TrashEntry {
    return JSON.parse(String(this.sql.value('SELECT textdb_trash_entry(?)', id))) as TrashEntry;
  }

  /** A trashed file's content at `version`, or as it was when deleted. */
  trashRead(id: number, version?: number): string {
    return asText(this.sql.value('SELECT textdb_trash_content(?, ?)', id, version ?? null));
  }

  trashHistory(id: number): HistoryEntry[] {
    return JSON.parse(String(this.sql.value('SELECT textdb_trash_history(?)', id))) as HistoryEntry[];
  }

  /** Removes a trash entry, and everything deleted with it inside, for good. */
  purge(id: number, options: AuthorOptions = {}): PurgeStats {
    return JSON.parse(String(this.sql.value('SELECT textdb_purge(?, ?)', id, this.authorOf(options)))) as PurgeStats;
  }

  /** Purges every trash item. */
  emptyTrash(options: AuthorOptions = {}): PurgeStats {
    return JSON.parse(String(this.sql.value('SELECT textdb_empty_trash(?)', this.authorOf(options)))) as PurgeStats;
  }

  /** What `target` holds: one file, or a folder with the files and folders anywhere below it. */
  stat(target: string): Stat {
    const node = this.sql.get<{ path: string; kind: 'file' | 'folder'; nbytes: number | null }>(
      'SELECT path, kind, nbytes FROM kb WHERE path = ?',
      target,
    );
    if (!node) throw new NotFound(`not found: ${target}`);
    if (node.kind === 'file') return { path: node.path, kind: 'file', files: 1, folders: 0, nbytes: node.nbytes ?? 0 };
    // Everything strictly below the folder sorts between "<folder>/" and "<folder>0".
    const base = node.path === '/' ? '' : node.path;
    const below = this.sql.get<{ files: number; folders: number; nbytes: number }>(
      `SELECT count(*) FILTER (WHERE kind = 'file') AS files, count(*) FILTER (WHERE kind = 'folder') AS folders,
              coalesce(sum(nbytes), 0) AS nbytes
         FROM kb WHERE path > ? AND path < ?`,
      `${base}/`,
      `${base}0`,
    );
    return { path: node.path, kind: 'folder', files: Number(below?.files ?? 0), folders: Number(below?.folders ?? 0), nbytes: Number(below?.nbytes ?? 0) };
  }

  lastSeq(): number {
    return Number(this.sql.value('SELECT textdb_last_seq()'));
  }

  /** Changes with `seq > since`, oldest first. */
  feed(since: number, limit?: number): Change[] {
    const args = limit === undefined ? [since] : [since, limit];
    return this.sql.all<Change>(`SELECT * FROM textdb_feed(${placeholders(args)})`, ...args);
  }

  /**
   * Creates or updates many files in one transaction, recorded with the message `import`.
   * Unchanged files make no new version. A file the store refuses (a bad path, a folder in
   * the way) is reported and the rest still land: each write runs under its own savepoint.
   */
  importBatch(files: readonly ImportFile[], options: AuthorOptions = {}): ImportStats {
    const stats: ImportStats = { created: 0, updated: 0, unchanged: 0, failed: 0, failures: [] };
    this.transaction(() => {
      for (const file of files) {
        try {
          const result = this.write(file.path, file.content, { author: options.author, message: 'import' });
          if (result.kind === 'noop') stats.unchanged++;
          else if (result.version === 1) stats.created++;
          else stats.updated++;
        } catch (error) {
          if (!(error instanceof TextdbError)) throw error;
          stats.failed++;
          stats.failures.push({ path: file.path, code: error.code, message: error.message });
        }
      }
    });
    return stats;
  }

  /** Runs `fn` inside `BEGIN IMMEDIATE … COMMIT`, rolling back if it throws. */
  transaction<T>(fn: () => T): T {
    this.sql.exec('BEGIN IMMEDIATE');
    try {
      const result = fn();
      this.sql.exec('COMMIT');
      return result;
    } catch (error) {
      this.sql.exec('ROLLBACK');
      throw error;
    }
  }

  /** Watches the store for commits from any connection, this one included. */
  watch(options: WatchOptions): Watcher {
    if (this.db === ':memory:') throw new TextdbError('an in-memory store cannot be watched');
    return new Watcher(this.db, this.extension, options);
  }

  close(): void {
    this.sql.conn.close();
  }

  private authorOf(options: AuthorOptions): string | null {
    return options.author ?? this.author;
  }

  private optionalAuthor(options: AuthorOptions): string[] {
    const author = this.authorOf(options);
    return author === null ? [] : [author];
  }
}

function parseWrite(json: unknown): WriteResult {
  return JSON.parse(asText(json)) as WriteResult;
}

/** The store counts a line per newline. */
function countNewlines(text: string): number {
  let n = 0;
  for (let i = text.indexOf('\n'); i >= 0; i = text.indexOf('\n', i + 1)) n++;
  return n;
}
