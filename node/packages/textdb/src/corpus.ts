import path from 'node:path';
import { openConnection, resolveExtension } from './connection.ts';
import { InvalidEdit, NotFound, TextdbError } from './errors.ts';
import { Sql, asText, placeholders } from './sql.ts';
import {
  type AuthorCount,
  type Change,
  type Chunk,
  type Entry,
  type FileView,
  type HistoryEntry,
  type Hunk,
  type Info,
  type ListOptions,
  type ListPage,
  type PropertyHit,
  type HeadingMatch,
  type HeadingName,
  type OutlineEntry,
  type PropertyKey,
  type PropertyValue,
  SORT_KEYS,
  type Link,
  type LinkOptions,
  type SearchHit,
  type SortKey,
  type WriteResult,
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

/** A rename, move or delete as it touched one file or folder. */
export interface PathEvent {
  id: number;
  ts: string;
  op: 'rename' | 'move' | 'delete';
  old_path: string;
  /** Where it went; null for a delete. */
  new_path: string | null;
  /** The folder the operation named, when this file or folder went along with it. */
  via: string | null;
  /** A file's version when it happened. */
  version: number | null;
  author: string | null;
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

/** The last `textdb sync` of a folder with a directory. */
export interface SyncState {
  dir: string;
  /** The store's last change number after the sync. */
  seq: number;
  synced_at: string;
  author: string | null;
  /** The checkout the directory was in; null when it is not in one. */
  git: { commit: string | null; branch: string | null; remote: string | null; clean: boolean } | null;
  /** Files changed, added or deleted in the store since. */
  changed: number;
  /** Files the sync wrote conflict markers into. */
  conflicts: string[];
}

interface SyncRow {
  id: number;
  dir: string;
  seq: number;
  synced_at: string;
  author: string | null;
  git_commit: string | null;
  git_branch: string | null;
  git_remote: string | null;
  git_clean: number | null;
}

/** A file an export writes, relative to the exported folder. */
export interface ExportFile {
  path: string;
  /** `/`-separated, below the exported folder. */
  rel: string;
  nbytes: number;
  updated_at: string;
}

export interface BulkOptions extends AuthorOptions {
  /** A move's destination folder; created when missing. */
  to?: string;
}

export interface BulkResult {
  op: 'move' | 'delete';
  /** A move's destination folder; null for a delete. */
  to: string | null;
  /** The paths moved or deleted, sorted. */
  done: string[];
  /** Paths that went with a listed folder, or were already in the destination. */
  skipped: string[];
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

/** The canonical `Entry` columns, in order. One list, so `ls`, `list` and `entry` agree. */
const ENTRY_COLS =
  'path, name, kind, version, nbytes, nlines, updated_at, updated_by, id, dir, depth, ext, title, ' +
  'nwords, nsections, nprops, nlinks, nlinks_broken, versions, created_at, files, folders, nauthors, authors';

type EntryRow = Omit<Entry, 'authors'> & { authors: string };

function toEntry({ authors, ...row }: EntryRow & { total?: number }): Entry {
  delete row.total;
  return { ...row, authors: JSON.parse(authors) as AuthorCount[] };
}

/** A file's extension, lower-cased, '' for folders and names without one (SQL over `textdb_ls` rows). */
const EXTENSION_SQL =
  "CASE WHEN kind = 'folder' OR instr(name, '.') = 0 THEN '' ELSE lower(replace(name, rtrim(name, replace(name, '.', '')), '')) END";

const SORT_SQL: Record<SortKey, string> = {
  name: 'name COLLATE NOCASE',
  type: EXTENSION_SQL,
  size: 'nbytes',
  lines: 'nlines',
  words: 'nwords',
  versions: 'versions',
  created: 'created_at',
  updated: 'updated_at',
  authors: 'coalesce(nauthors, 0)',
};

const MAX_PAGE = 1000;

/** A LIKE pattern: a glob when `q` has `*` or `?`, otherwise "contains". */
export function namePattern(q: string): string {
  const escaped = q.replace(/[\\%_]/g, (c) => `\\${c}`);
  return /[*?]/.test(q) ? escaped.replace(/\*/g, '%').replace(/\?/g, '_') : `%${escaped}%`;
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
    const rows = this.sql.all<EntryRow>(`SELECT ${ENTRY_COLS} FROM textdb_ls(?)`, dir);
    return rows.map(toEntry).sort((a, b) => Number(a.kind === 'file') - Number(b.kind === 'file'));
  }

  /** A page of a folder's entries (or, recursive, of everything below it), sorted and filtered in the store. */
  list(dir = '/', options: ListOptions = {}): ListPage {
    const key = options.sort ?? 'name';
    if (!SORT_KEYS.includes(key)) throw new InvalidEdit(`sort must be one of ${SORT_KEYS.join(', ')}`);
    const order = options.order === 'desc' ? 'DESC' : 'ASC';
    const offset = Math.max(0, Math.trunc(options.offset ?? 0));
    const limit = Math.min(MAX_PAGE, Math.max(1, Math.trunc(options.limit ?? 200)));
    const where: string[] = [];
    const params: (string | number)[] = [dir, options.recursive ? 1 : 0];
    if (options.name) {
      where.push("name LIKE ? ESCAPE '\\'");
      params.push(namePattern(options.name));
    }
    if (options.author !== undefined) {
      where.push("EXISTS (SELECT 1 FROM json_each(authors) WHERE coalesce(json_extract(value, '$.author'), '') = ?)");
      params.push(options.author);
    }
    if (options.type) {
      where.push(`${EXTENSION_SQL} = lower(?)`);
      params.push(options.type.replace(/^\./, ''));
    }
    if (options.kind) {
      where.push('kind = ?');
      params.push(options.kind);
    }
    const filter = where.length ? `WHERE ${where.join(' AND ')}` : '';
    // A recursive listing keeps each folder with what is inside it; one level lists folders first.
    const groups = options.recursive ? '' : "kind = 'file', ";
    const tie = options.recursive ? 'path' : 'name';
    const rows = this.sql.all<EntryRow & { total: number }>(
      `SELECT ${ENTRY_COLS}, count(*) OVER () AS total FROM textdb_ls(?, ?) ${filter}
       ORDER BY ${groups}${SORT_SQL[key]} ${order}, ${tie} ${order} LIMIT ? OFFSET ?`,
      ...params,
      limit,
      offset,
    );
    const total = rows[0]?.total ?? Number(this.sql.value(`SELECT count(*) FROM textdb_ls(?, ?) ${filter}`, ...params));
    return { path: dir, total, offset, limit, entries: rows.map(toEntry) };
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
      'SELECT version, author, ts, message, kind, base_version, nbytes, nlines, nwords FROM textdb_history(?)',
      filePath,
    );
  }

  /** Renames, moves and deletes of the file or folder at `filePath`, oldest first. */
  pathHistory(filePath: string): PathEvent[] {
    return this.sql.all<PathEvent>(
      'SELECT id, ts, op, old_path, new_path, via, version, author FROM textdb_path_history(?)',
      filePath,
    );
  }

  /** As `pathHistory`, for the node with this id (a trash entry). */
  pathHistoryOf(id: number): PathEvent[] {
    return this.sql.all<PathEvent>(
      'SELECT id, ts, op, old_path, new_path, via, version, author FROM textdb_path_history(NULL, ?)',
      id,
    );
  }

  /** A store setting (`path_history`); null at its default. */
  setting(key: string): string | null {
    return (this.sql.value('SELECT textdb_setting(?)', key) as string | null) ?? null;
  }

  /** Sets a store setting, or returns it to its default with null; answers the stored value. */
  setSetting(key: string, value: string | null): string | null {
    return (this.sql.value('SELECT textdb_setting(?, ?)', key, value) as string | null) ?? null;
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

  /**
   * Matching lines. One row per line, `limit` counting rows and `perFile` capping how many
   * come from any one document — the same meanings the CLI gives those words.
   */
  search(query: string, options: { prefix?: string; limit?: number; perFile?: number } = {}): SearchHit[] {
    return this.sql.all<SearchHit>(
      'SELECT path, version, line, text, section, score, more FROM textdb_search(?, ?, ?, ?)',
      query,
      options.prefix ?? '/',
      // One default for the whole project: 200 rows. It used to be 100 in SQL, 50 here and
      // in HTTP, and 200 in the UI, for the same function.
      options.limit ?? 200,
      options.perFile ?? 10,
    );
  }

  /** One path's listing row — the record `textdb stat` prints. */
  entry(target: string): Entry {
    const row = this.sql.get<EntryRow>(`SELECT ${ENTRY_COLS} FROM textdb_entry(?)`, target);
    if (!row) throw new NotFound(`not found: ${target}`);
    return toEntry(row);
  }

  /**
   * Front-matter property names in use, most-used first.
   *
   * `prefix` is what the user has typed: this runs on every keystroke, so it is an index
   * range rather than a scan.
   */
  propertyKeys(options: { prefix?: string; limit?: number } = {}): PropertyKey[] {
    return this.sql
      .all<{ key: string; docs: number; values_n: number; kind: PropertyKey['kind'] }>(
        'SELECT key, docs, values_n, kind FROM textdb_prop_keys(?, ?)',
        options.prefix ?? '',
        options.limit ?? 200,
      )
      .map((r) => ({ key: r.key, docs: r.docs, values_n: r.values_n, kind: r.kind }));
  }

  /**
   * Markdown headings under `path`: one document's outline, everything below a folder, or
   * the whole store with `/`.
   *
   * Each row carries its document's size, line and word counts and last change, so a table
   * needs no second query per row. `heading` narrows to one heading, matched folded, with
   * `match` choosing `exact`, `prefix` or `contains`; `level` caps the depth.
   */
  outline(
    path = '/',
    options: { heading?: string; match?: HeadingMatch; level?: number; limit?: number } = {},
  ): OutlineEntry[] {
    return this.sql
      .all<{
        path: string;
        heading: string;
        heading_path: string;
        level: number;
        line_from: number;
        line_to: number;
        nwords: number | null;
        nwords_total: number | null;
        nbytes: number | null;
        nlines: number | null;
        file_nwords: number | null;
        version: number;
        updated_at: string;
        updated_by: string | null;
      }>(
        'SELECT path, heading, heading_path, level, line_from, line_to, nwords, nwords_total, ' +
          'nbytes, nlines, file_nwords, version, updated_at, updated_by FROM textdb_outline(?, ?, ?, ?, ?)',
        path,
        options.heading ?? null,
        options.match ?? 'exact',
        options.level ?? null,
        options.limit ?? 1000,
      )
      .map((r) => ({
        path: r.path,
        heading: r.heading,
        headingPath: r.heading_path,
        level: r.level,
        lineFrom: r.line_from,
        lineTo: r.line_to,
        nwords: r.nwords,
        nwordsTotal: r.nwords_total,
        nbytes: r.nbytes,
        nlines: r.nlines,
        fileNwords: r.file_nwords,
        version: r.version,
        updated_at: r.updated_at,
        updatedBy: r.updated_by,
      }));
  }

  /**
   * Distinct headings in use, most-used first — the autosuggest call for outlines.
   *
   * `starts` is what the user has typed, matched as a folded prefix against an index range.
   */
  headingNames(path = '/', options: { starts?: string; limit?: number } = {}): HeadingName[] {
    return this.sql.all<HeadingName>(
      'SELECT heading, sections, docs FROM textdb_headings(?, ?, ?)',
      path,
      options.starts ?? '',
      options.limit ?? 100,
    );
  }

  /**
   * Links written in a document, or in everything below a folder.
   *
   * `status` keeps only one kind — `broken` is the one worth asking for. Rows come back in
   * document order within a document and in path order across them.
   */
  links(path = '/', options: LinkOptions = {}): Link[] {
    return this.linkRows('textdb_links', path, options);
  }

  /**
   * The links pointing at a document, as `links` gives the ones leaving it.
   *
   * An asset is found by its own path, not by the `.tdbasset` pointer beside it.
   */
  backlinks(path = '/', options: LinkOptions = {}): Link[] {
    return this.linkRows('textdb_backlinks', path, options);
  }

  /** Both directions read the same ten columns, so neither can drift from the other. */
  private linkRows(fn: 'textdb_links' | 'textdb_backlinks', path: string, options: LinkOptions): Link[] {
    return this.sql
      .all<Omit<Link, 'asset'> & { asset: number }>(
        `SELECT path, version, line, kind, target, anchor, alias, status, resolved, asset FROM ${fn}(?, ?, ?)`,
        path,
        options.status ?? '',
        options.limit ?? 10000,
      )
      .map((r) => ({ ...r, asset: r.asset !== 0 }));
  }

  /** The values one property takes, most-used first; `prefix` narrows them as above. */
  propertyValues(key: string, options: { prefix?: string; limit?: number } = {}): PropertyValue[] {
    return this.sql.all<PropertyValue>(
      'SELECT value, docs FROM textdb_prop_values(?, ?, ?)',
      key,
      options.prefix ?? '',
      options.limit ?? 200,
    );
  }

  /**
   * Documents matching a property query: `status:draft tags:telco -priority:>3`.
   *
   * See `textdb_md::query` for the grammar. A malformed query raises, with the offset it
   * went wrong at in the message, so an editor can point at it.
   */
  propertyFind(query: string, options: { folder?: string; limit?: number } = {}): PropertyHit[] {
    return this.sql
      .all<{ path: string; nbytes: number; updated_at: string; frontmatter: string | null }>(
        'SELECT path, nbytes, updated_at, frontmatter FROM textdb_prop_find(?, ?, ?)',
        query,
        options.folder ?? '/',
        options.limit ?? 500,
      )
      .map((r) => ({
        path: r.path,
        nbytes: r.nbytes,
        updated_at: r.updated_at,
        // Parsed here so every caller does not: the column is the JSON the store keeps, and a
        // document whose front matter failed to parse is reported as having none rather than
        // failing the whole query.
        frontmatter: r.frontmatter ? safeJson(r.frontmatter) : null,
      }));
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

  /** Every live file below the folder `dir`, by path, with its size and last change: what an export writes. */
  exportFiles(dir = '/'): ExportFile[] {
    const folder = this.entry(dir);
    if (folder.kind !== 'folder') throw new InvalidEdit(`${folder.path} is a file, not a folder`);
    const base = folder.path === '/' ? '' : folder.path;
    // Everything strictly below the folder sorts between "<folder>/" and "<folder>0".
    return this.sql
      .all<{ path: string; nbytes: number; updated_at: string }>(
        "SELECT path, nbytes, updated_at FROM kb WHERE kind = 'file' AND path > ? AND path < ? ORDER BY path",
        `${base}/`,
        `${base}0`,
      )
      .map((f) => ({ ...f, rel: f.path.slice(base.length + 1) }));
  }

  /**
   * A file's content exactly as stored — the bytes an import read, line endings and byte-order
   * mark included — whatever their encoding.
   */
  readBytes(filePath: string): Uint8Array {
    const row = this.sql.get<{ content: unknown }>("SELECT content FROM kb WHERE path = ? AND kind = 'file'", filePath);
    if (!row) throw new NotFound(`not found: ${filePath}`);
    if (row.content instanceof Uint8Array) return row.content;
    return new TextEncoder().encode(typeof row.content === 'string' ? row.content : '');
  }

  /**
   * What `textdb sync` recorded for the folder `prefix` and the directory `dir` (named as the CLI
   * names it: absolute, links resolved): when, at which git commit, how many files changed in the
   * store since, and which files it left conflict markers in. `null` before the first sync.
   */
  syncState(prefix: string, dir: string): SyncState | null {
    let row: SyncRow | undefined;
    try {
      row = this.sql.get<SyncRow>(
        'SELECT id, dir, seq, synced_at, author, git_commit, git_branch, git_remote, git_clean FROM kb_sync ' +
          'WHERE prefix = ? AND (dir = ? OR (? AND lower(dir) = lower(?)))',
        prefix,
        dir,
        process.platform === 'win32' ? 1 : 0,
        dir,
      );
    } catch (error) {
      // The CLI creates the sync tables the first time it opens the store.
      if (error instanceof Error && /no such table/i.test(error.message)) return null;
      throw error;
    }
    if (!row) return null;
    const base = prefix === '/' ? '' : prefix;
    // Files whose version is not the one synced, plus synced files no longer in the store.
    const changed = Number(
      this.sql.value(
        `SELECT (SELECT count(*) FROM kb_node n LEFT JOIN kb_sync_file f ON f.sync_id = ?1 AND f.rel = substr(n.path, ?2)
                  WHERE n.kind = 1 AND n.deleted_at IS NULL AND n.path > ?3 AND n.path < ?4
                    AND (f.version IS NULL OR f.version <> n.version))
              + (SELECT count(*) FROM kb_sync_file f WHERE f.sync_id = ?1
                  AND NOT EXISTS (SELECT 1 FROM kb_node n WHERE n.path = ?3 || f.rel AND n.deleted_at IS NULL))`,
        row.id,
        base.length + 2,
        `${base}/`,
        `${base}0`,
      ),
    );
    const conflicts = this.sql
      .all<{ rel: string }>('SELECT rel FROM kb_sync_file WHERE sync_id = ? AND conflict = 1 ORDER BY rel', row.id)
      .map((r) => r.rel);
    return {
      dir: row.dir,
      seq: row.seq,
      synced_at: row.synced_at,
      author: row.author,
      git:
        row.git_clean === null
          ? null
          : { commit: row.git_commit, branch: row.git_branch, remote: row.git_remote, clean: row.git_clean === 1 },
      changed,
      conflicts,
    };
  }

  /**
   * Moves several files and folders into the folder `to`, keeping their names, or deletes them —
   * in one transaction, so a failure leaves all of them as they were. A path inside another
   * listed folder goes along with that folder and is reported as skipped, as is a move to where
   * a path already is.
   */
  bulk(op: 'move' | 'delete', paths: readonly string[], options: BulkOptions = {}): BulkResult {
    const unique = [...new Set(paths)].sort();
    const covered = (p: string) => unique.some((q) => q !== p && (q === '/' || p.startsWith(`${q}/`)));
    const to = op === 'move' ? `/${(options.to ?? '').split('/').filter(Boolean).join('/')}` : null;
    if (op === 'move' && !options.to) throw new InvalidEdit('a bulk move needs a destination folder');
    const done: string[] = [];
    const skipped: string[] = [];
    this.transaction(() => {
      for (const p of unique) {
        const name = p.slice(p.lastIndexOf('/') + 1);
        const target = to === null ? null : `${to === '/' ? '' : to}/${name}`;
        if (covered(p) || target === p) {
          skipped.push(p);
          continue;
        }
        try {
          if (target === null) this.remove(p, options);
          else this.move(p, target, options);
        } catch (error) {
          if (error instanceof Error) error.message = `${p}: ${error.message}`;
          throw error;
        }
        done.push(p);
      }
    });
    return { op, to, done, skipped };
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

/** Parse JSON, or `null` rather than throwing: one unreadable row must not fail a search. */
function safeJson(text: string): Record<string, unknown> | null {
  try {
    const v: unknown = JSON.parse(text);
    return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}
