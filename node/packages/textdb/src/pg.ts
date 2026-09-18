import pg from 'pg';
import type { Capabilities, ChangeWatcher, CorpusApi } from './api.ts';
import type {
  AuthorOptions,
  BulkOptions,
  BulkResult,
  ExportFile,
  ImportFile,
  ImportStats,
  PathEvent,
  PurgeStats,
  Stat,
  SyncState,
  TrashEntry,
  WriteOptions,
} from './corpus.ts';
import { InvalidEdit, NotFound, TextdbError, Unsupported, fromMessage } from './errors.ts';
import {
  type Change,
  type Chunk,
  type Entry,
  type FileView,
  type HeadingName,
  type HistoryEntry,
  type Hunk,
  type Info,
  type Link,
  type LinkOptions,
  type ListOptions,
  type ListPage,
  type OutlineEntry,
  type PropertyHit,
  type PropertyKey,
  type PropertyValue,
  type SearchHit,
  SORT_KEYS,
  type Share,
  type SortKey,
  type Whoami,
  type WriteResult,
} from './types.ts';
import type { WatchOptions } from './watch.ts';

/**
 * `int8` arrives from node-postgres as a **string** by default, because a 64-bit integer does not
 * fit a JS number. Every count, size and version in this API is typed `number`, and nothing
 * downstream would notice the difference until arithmetic or a `deepEqual` failed -- so the parse
 * is set here, once, for this module's own pools.
 *
 * `Number` is exact to 2^53, which bounds a store at nine quadrillion bytes or versions. The
 * alternative is a `bigint` in the public shape, which would be a different contract from the
 * SQLite side and from `docs/shapes.md`.
 */
pg.types.setTypeParser(pg.types.builtins.INT8, (v: string) => Number(v));
/** `numeric`, which `score` comes back as. */
pg.types.setTypeParser(pg.types.builtins.NUMERIC, (v: string) => Number(v));

/** ISO-8601 UTC with milliseconds and `Z`, rendered in SQL rather than by the session's time zone. */
const utc = (col: string) => `to_char(${col} AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')`;

/** The canonical `Entry` columns of `kb.entry`, in the order `docs/shapes.md` fixes. */
const ENTRY_COLS =
  `e.path, e.name, e.kind, e.version, e.nbytes, e.nlines, ${utc('e.updated_at')} AS updated_at, e.updated_by, ` +
  `e.id, e.dir, e.depth, e.ext, e.title, e.nwords, e.nsections, e.nprops, e.nlinks, e.nlinks_broken, e.versions, ` +
  `${utc('e.created_at')} AS created_at, e.files, e.folders, e.nauthors, e.authors::text AS authors, ` +
  `e.share, e.rights, e.shares::text AS shares`;

const MAX_PAGE = 1000;

/** What a listing row looks like on the wire, before the two JSON columns are parsed. */
type EntryRow = Omit<Entry, 'authors' | 'shares'> & { authors: string | null; shares: string | null };

function toEntry(row: EntryRow & { total?: number }): Entry {
  const { authors, shares, ...rest } = row;
  delete rest.total;
  return {
    ...rest,
    authors: authors ? (JSON.parse(authors) as Entry['authors']) : [],
    shares: shares ? (JSON.parse(shares) as Share[]) : [],
  };
}

const SORT_SQL: Record<SortKey, string> = {
  name: 'e.name',
  type: "coalesce(e.ext, '')",
  size: 'e.nbytes',
  lines: 'e.nlines',
  words: 'e.nwords',
  versions: 'e.versions',
  created: 'e.created_at',
  updated: 'e.updated_at',
  authors: 'coalesce(e.nauthors, 0)',
};

/**
 * A Postgres error as a textdb one.
 *
 * The extension raises with `ERRCODE = 'TX00n'`, and a refusal from inside SPI arrives as `XX000`
 * with the code in front of the message -- the same two shapes the CLI's pg module reads.
 */
function toError(error: unknown): TextdbError {
  if (error instanceof TextdbError) return error;
  const e = error as { code?: string; message?: string };
  const message = e?.message ?? String(error);
  if (typeof e?.code === 'string' && /^TX\d{3}$/.test(e.code)) {
    return fromMessage(`${e.code} ${message.replace(new RegExp(`^${e.code}\\s*`), '')}`) ?? new TextdbError(message, e.code as never);
  }
  return fromMessage(message) ?? new TextdbError(message);
}

/** A store URL with its password taken out, for anything that reports what it is connected to. */
export function redactUrl(url: string): string {
  try {
    const parsed = new URL(url);
    if (parsed.password) parsed.password = '***';
    return parsed.toString();
  } catch {
    // Not a URL this build can parse: say nothing rather than risk printing a secret.
    return url.replace(/\/\/[^@/]*@/, '//***@');
  }
}

/** The number of newlines in `text`, which is what `nlines` counts. */
function countNewlines(text: string): number {
  let n = 0;
  for (let i = text.indexOf('\n'); i >= 0; i = text.indexOf('\n', i + 1)) n++;
  return n;
}

/**
 * A corpus over a Postgres store, through the extension's `kb.*` surface and nothing else.
 *
 * The rule the CLI's pg module states about itself holds here too: **every operation is a call
 * into `kb.*`**, never a reach around it to a table. `kb.entry` and `kb.file` already answer in
 * the caller's own paths and filter to what the caller may see; a query against `kb.node` would
 * walk straight past every filter #12 adds. The two exceptions are `kb.sync` and `kb.sync_file`,
 * which are the CLI's own bookkeeping and have no function over them on either engine.
 */
export class PgCorpus implements CorpusApi {
  readonly backend = 'postgres' as const;
  readonly db: string;
  readonly capabilities: Capabilities = {
    // The trash and one-batch undo read shadow tables that only the SQLite schema has.
    trash: false,
    revertBatch: false,
    syncState: true,
  };
  readonly author: string | null;
  private readonly client: pg.Client;
  private accountName: string | null = null;
  private closed = false;

  private constructor(client: pg.Client, url: string, author: string | null) {
    this.client = client;
    this.db = redactUrl(url);
    this.author = author;
  }

  /** Connects, and presents `token` if there is one. */
  static async open(options: { url: string; author?: string; token?: string }): Promise<PgCorpus> {
    const client = new pg.Client({ connectionString: options.url });
    try {
      await client.connect();
    } catch (error) {
      throw toError(error);
    }
    const corpus = new PgCorpus(client, options.url, options.author ?? null);
    try {
      // Fails loudly rather than answering half a surface: a store without the extension has no
      // `kb` schema at all, and every later call would say so one at a time.
      await corpus.one<{ ok: boolean }>("SELECT to_regclass('kb.entry') IS NOT NULL AS ok");
      if (options.token !== undefined) await corpus.authenticate(options.token);
    } catch (error) {
      await corpus.close();
      throw error;
    }
    return corpus;
  }

  private async rows<T>(sql: string, params: unknown[] = []): Promise<T[]> {
    if (this.closed) throw new TextdbError('this corpus is closed');
    try {
      const result = await this.client.query(sql, params);
      return result.rows as T[];
    } catch (error) {
      throw toError(error);
    }
  }

  private async one<T>(sql: string, params: unknown[] = []): Promise<T | undefined> {
    const rows = await this.rows<T>(sql, params);
    return rows[0];
  }

  /** The first column of the first row. */
  private async value<T>(sql: string, params: unknown[] = []): Promise<T | undefined> {
    if (this.closed) throw new TextdbError('this corpus is closed');
    try {
      const result = await this.client.query(sql, params);
      const row = result.rows[0] as Record<string, unknown> | undefined;
      return row === undefined ? undefined : (Object.values(row)[0] as T);
    } catch (error) {
      throw toError(error);
    }
  }

  private authorOf(options: { author?: string }): string | null {
    return options.author ?? this.author;
  }

  get account(): string | null {
    return this.accountName;
  }

  async authenticate(bearer: string | null): Promise<string | null> {
    if (bearer === null) {
      // Back to the owner. `kb.current_account()` reads the GUC, so clearing it is the whole of it.
      await this.value("SELECT set_config('textdb.token', '', false)");
      this.accountName = null;
      return null;
    }
    const name = await this.value<string | null>('SELECT kb.auth($1)', [bearer]);
    this.accountName = name && name.length > 0 ? name : null;
    return this.accountName;
  }

  async whoami(): Promise<Whoami> {
    const rows = await this.rows<{
      account: string | null;
      admin: boolean;
      kind: string;
      namespace: string;
      alias: string | null;
      rights: string | null;
      node_id: number | null;
      dormant: boolean | null;
    }>('SELECT account, admin, kind, namespace, alias, rights, node_id, dormant FROM kb.whoami() ORDER BY alias');
    const first = rows[0];
    if (!first) throw new TextdbError('kb.whoami() said nothing');
    return {
      account: first.account,
      admin: Boolean(first.admin),
      kind: first.kind,
      namespace: first.namespace,
      shares: rows
        .filter((r) => r.alias !== null)
        .map((r) => ({ alias: r.alias as string, rights: r.rights ?? '', node_id: Number(r.node_id), dormant: Boolean(r.dormant) })),
    };
  }

  async info(): Promise<Info> {
    const files = await this.value<number>("SELECT count(*) FROM kb.entry e WHERE e.kind = 'file'");
    return { db: this.db, files: Number(files ?? 0), last_seq: await this.lastSeq() };
  }

  async lastSeq(): Promise<number> {
    return Number((await this.value<number>('SELECT kb.last_seq()')) ?? 0);
  }

  async feed(since: number, limit?: number): Promise<Change[]> {
    return this.rows<Change>(
      `SELECT seq, ${utc('ts')} AS ts, op, path, old_path, node_kind, version, base_version, commit_kind, author, message
         FROM kb.feed($1, $2)`,
      [since, limit ?? 1000],
    );
  }

  watch(options: WatchOptions): ChangeWatcher {
    return new PgWatcher(this, options);
  }

  async ls(dir = '/'): Promise<Entry[]> {
    const rows = await this.rows<EntryRow>(`SELECT ${ENTRY_COLS} FROM kb.ls($1) e`, [dir]);
    return rows.map(toEntry).sort((a, b) => Number(a.kind === 'file') - Number(b.kind === 'file'));
  }

  async list(dir = '/', options: ListOptions = {}): Promise<ListPage> {
    const key = options.sort ?? 'name';
    if (!SORT_KEYS.includes(key)) throw new InvalidEdit(`sort must be one of ${SORT_KEYS.join(', ')}`);
    const order = options.order === 'desc' ? 'DESC' : 'ASC';
    const offset = Math.max(0, Math.trunc(options.offset ?? 0));
    const limit = Math.min(MAX_PAGE, Math.max(1, Math.trunc(options.limit ?? 200)));
    const where: string[] = [];
    const params: unknown[] = [dir, options.recursive === true];
    const next = () => `$${params.length}`;
    if (options.name) {
      params.push(namePattern(options.name));
      where.push(`e.name LIKE ${next()} ESCAPE '\\'`);
    }
    if (options.author !== undefined) {
      params.push(options.author);
      where.push(
        `EXISTS (SELECT 1 FROM jsonb_array_elements(e.authors) a WHERE coalesce(a->>'author', '') = ${next()})`,
      );
    }
    if (options.type) {
      params.push(options.type.replace(/^\./, ''));
      where.push(`coalesce(e.ext, '') = lower(${next()})`);
    }
    if (options.kind) {
      params.push(options.kind);
      where.push(`e.kind = ${next()}`);
    }
    const filter = where.length ? `WHERE ${where.join(' AND ')}` : '';
    // A recursive listing keeps each folder with what is inside it; one level lists folders first.
    const groups = options.recursive ? '' : `(e.kind = 'file') ${order}, `;
    const tie = options.recursive ? 'e.path' : 'e.name';
    params.push(limit, offset);
    const rows = await this.rows<EntryRow & { total: number }>(
      `SELECT ${ENTRY_COLS}, count(*) OVER () AS total FROM kb.ls($1, $2) e ${filter}
       ORDER BY ${groups}${SORT_SQL[key]} ${order}, ${tie} ${order} LIMIT $${params.length - 1} OFFSET $${params.length}`,
      params,
    );
    const total = rows[0]?.total ?? 0;
    return { path: dir, total: Number(total), offset, limit, entries: rows.map(toEntry) };
  }

  async entry(target: string): Promise<Entry> {
    const row = await this.one<EntryRow>(`SELECT ${ENTRY_COLS} FROM kb.entry e WHERE e.path = $1`, [target]);
    if (!row) throw new NotFound(`not found: ${target}`);
    return toEntry(row);
  }

  async stat(target: string): Promise<Stat> {
    const node = await this.one<{ path: string; kind: 'file' | 'folder'; nbytes: number | null }>(
      'SELECT e.path, e.kind, e.nbytes FROM kb.entry e WHERE e.path = $1',
      [target],
    );
    if (!node) throw new NotFound(`not found: ${target}`);
    if (node.kind === 'file') return { path: node.path, kind: 'file', files: 1, folders: 0, nbytes: Number(node.nbytes ?? 0) };
    const base = node.path === '/' ? '' : node.path;
    const below = await this.one<{ files: number; folders: number; nbytes: number }>(
      `SELECT count(*) FILTER (WHERE e.kind = 'file') AS files, count(*) FILTER (WHERE e.kind = 'folder') AS folders,
              coalesce(sum(e.nbytes), 0) AS nbytes
         FROM kb.entry e WHERE e.path > $1 AND e.path < $2`,
      [`${base}/`, `${base}0`],
    );
    return {
      path: node.path,
      kind: 'folder',
      files: Number(below?.files ?? 0),
      folders: Number(below?.folders ?? 0),
      nbytes: Number(below?.nbytes ?? 0),
    };
  }

  async read(filePath: string, version?: number): Promise<FileView> {
    const head = await this.one<{
      version: number;
      content: string;
      nbytes: number;
      nlines: number;
      updated_at: string;
      updated_by: string | null;
    }>(
      `SELECT f.version, f.content, f.nbytes, f.nlines, ${utc('f.updated_at')} AS updated_at, f.updated_by
         FROM kb.file f WHERE f.path = $1`,
      [filePath],
    );
    if (!head) throw new NotFound(`not found: ${filePath}`);
    if (version === undefined || version === head.version) {
      const { content, ...rest } = head;
      return { path: filePath, head_version: head.version, content, ...rest };
    }
    const content = (await this.value<string>('SELECT kb.content($1, $2)', [filePath, version])) ?? '';
    const commit = await this.one<{ author: string | null; ts: string; nbytes: number }>(
      `SELECT author, ${utc('ts')} AS ts, nbytes FROM kb.history($1) WHERE version = $2`,
      [filePath, version],
    );
    if (!commit) throw new NotFound(`not found: version ${version} of ${filePath}`);
    return {
      path: filePath,
      version,
      head_version: head.version,
      content,
      nbytes: Number(commit.nbytes),
      nlines: countNewlines(content),
      updated_at: commit.ts,
      updated_by: commit.author,
    };
  }

  async readBytes(filePath: string): Promise<Uint8Array> {
    const row = await this.one<{ content: string }>('SELECT f.content FROM kb.file f WHERE f.path = $1', [filePath]);
    if (!row) throw new NotFound(`not found: ${filePath}`);
    // Postgres stores content as text, so the bytes are its UTF-8.
    return new TextEncoder().encode(row.content ?? '');
  }

  async chunks(filePath: string, version?: number): Promise<Chunk[]> {
    return this.rows<Chunk>('SELECT ord, hash, byte_from, nbytes, line_from, nlines FROM kb.chunks($1, $2)', [
      filePath,
      version ?? null,
    ]);
  }

  async history(filePath: string): Promise<HistoryEntry[]> {
    return this.rows<HistoryEntry>(
      `SELECT version, author, ${utc('ts')} AS ts, message, kind, base_version, nbytes, nlines, nwords
         FROM kb.history($1)`,
      [filePath],
    );
  }

  async hunks(filePath: string, from?: number, to?: number): Promise<Hunk[]> {
    const head = to ?? (await this.entry(filePath)).version ?? 1;
    const v1 = from ?? head - 1;
    return this.rows<Hunk>('SELECT old_from, old_count, new_from, new_count, old_text, new_text FROM kb.hunks($1, $2, $3)', [
      filePath,
      v1,
      head,
    ]);
  }

  async diff(filePath: string, from: number, to: number): Promise<string> {
    return (await this.value<string>('SELECT kb.diff($1, $2, $3)', [filePath, from, to])) ?? '';
  }

  async pathHistory(filePath: string): Promise<PathEvent[]> {
    return this.rows<PathEvent>(
      `SELECT id, ${utc('ts')} AS ts, op, old_path, new_path, via, version, author FROM kb.path_history($1)`,
      [filePath],
    );
  }

  async pathHistoryOf(id: number): Promise<PathEvent[]> {
    return this.rows<PathEvent>(
      `SELECT id, ${utc('ts')} AS ts, op, old_path, new_path, via, version, author FROM kb.path_history(NULL, $1)`,
      [id],
    );
  }

  async exportFiles(dir = '/'): Promise<ExportFile[]> {
    const folder = await this.entry(dir);
    if (folder.kind !== 'folder') throw new InvalidEdit(`${folder.path} is a file, not a folder`);
    const base = folder.path === '/' ? '' : folder.path;
    const rows = await this.rows<{ path: string; nbytes: number; updated_at: string }>(
      `SELECT e.path, e.nbytes, ${utc('e.updated_at')} AS updated_at
         FROM kb.entry e WHERE e.kind = 'file' AND e.path > $1 AND e.path < $2 ORDER BY e.path`,
      [`${base}/`, `${base}0`],
    );
    return rows.map((f) => ({ ...f, rel: f.path.slice(base.length + 1) }));
  }

  async search(query: string, options: { prefix?: string; limit?: number; perFile?: number } = {}): Promise<SearchHit[]> {
    return this.rows<SearchHit>('SELECT path, version, line, text, section, score::float8 AS score, more FROM kb.search($1, $2, $3, $4)', [
      query,
      options.prefix ?? '/',
      options.limit ?? 200,
      options.perFile ?? 10,
    ]);
  }

  async links(path = '/', options: LinkOptions = {}): Promise<Link[]> {
    return this.rows<Link>(
      'SELECT path, version, line, kind, target, anchor, alias, status, resolved, asset FROM kb.links($1, $2, $3)',
      // '' is "every status", as the function's own default says; NULL would match nothing.
      [path, options.status ?? '', options.limit ?? 10000],
    );
  }

  async backlinks(path = '/', options: LinkOptions = {}): Promise<Link[]> {
    return this.rows<Link>(
      'SELECT path, version, line, kind, target, anchor, alias, status, resolved, asset FROM kb.backlinks($1, $2, $3)',
      [path, options.status ?? '', options.limit ?? 10000],
    );
  }

  async outline(
    path = '/',
    options: { heading?: string; match?: 'exact' | 'prefix' | 'contains'; level?: number; limit?: number } = {},
  ): Promise<OutlineEntry[]> {
    const rows = await this.rows<{
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
      `SELECT path, heading, heading_path, level, line_from, line_to, nwords, nwords_total, nbytes, nlines,
              file_nwords, version, ${utc('updated_at')} AS updated_at, updated_by
         FROM kb.outline($1, $2, $3, $4, $5)`,
      [path, options.heading ?? null, options.match ?? 'exact', options.level ?? null, options.limit ?? 1000],
    );
    // The record is camelCase here, as the SQLite side builds it: one shape, whatever the SQL said.
    return rows.map((r) => ({
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

  async headingNames(path = '/', options: { starts?: string; limit?: number } = {}): Promise<HeadingName[]> {
    return this.rows<HeadingName>('SELECT heading, sections, docs FROM kb.headings($1, $2, $3)', [
      path,
      options.starts ?? '',
      options.limit ?? 100,
    ]);
  }

  async propertyKeys(options: { prefix?: string; limit?: number } = {}): Promise<PropertyKey[]> {
    return this.rows<PropertyKey>('SELECT key, docs, values_n, kind FROM kb.prop_keys($1, $2)', [
      options.prefix ?? '',
      options.limit ?? 200,
    ]);
  }

  async propertyValues(key: string, options: { prefix?: string; limit?: number } = {}): Promise<PropertyValue[]> {
    return this.rows<PropertyValue>('SELECT value, docs FROM kb.prop_values($1, $2, $3)', [
      key,
      options.prefix ?? '',
      options.limit ?? 200,
    ]);
  }

  async propertyFind(query: string, options: { folder?: string; limit?: number } = {}): Promise<PropertyHit[]> {
    const rows = await this.rows<{ path: string; nbytes: number; updated_at: string; frontmatter: string | null }>(
      'SELECT path, nbytes, updated_at, frontmatter FROM kb.prop_find($1, $2, $3)',
      [query, options.folder ?? '/', options.limit ?? 500],
    );
    return rows as PropertyHit[];
  }

  async write(filePath: string, content: string, options: WriteOptions = {}): Promise<WriteResult> {
    const json = await this.value<{ version: number; kind: WriteResult['kind'] }>('SELECT kb.write($1, $2, $3, $4, $5)', [
      filePath,
      content,
      options.baseVersion ?? null,
      this.authorOf(options),
      options.message ?? null,
    ]);
    return asWrite(json);
  }

  async replaceLines(
    filePath: string,
    from: number,
    to: number,
    text: string,
    options: Omit<WriteOptions, 'message'> = {},
  ): Promise<WriteResult> {
    const json = await this.value<{ version: number; kind: WriteResult['kind'] }>(
      'SELECT kb.replace_lines($1, $2, $3, $4, $5, $6)',
      [filePath, from, to, text, options.baseVersion ?? null, this.authorOf(options)],
    );
    return asWrite(json);
  }

  async edit(filePath: string, oldText: string, newText: string, options: AuthorOptions = {}): Promise<number> {
    return Number(await this.value<number>('SELECT kb.edit($1, $2, $3, $4)', [filePath, oldText, newText, this.authorOf(options)]));
  }

  async append(filePath: string, text: string, options: AuthorOptions = {}): Promise<number> {
    return Number(await this.value<number>('SELECT kb.append($1, $2, $3)', [filePath, text, this.authorOf(options)]));
  }

  async move(from: string, to: string, options: AuthorOptions = {}): Promise<void> {
    await this.value('SELECT kb.move($1, $2, $3)', [from, to, this.authorOf(options)]);
  }

  async remove(target: string, options: AuthorOptions = {}): Promise<void> {
    await this.value('SELECT kb.remove($1, $2)', [target, this.authorOf(options)]);
  }

  async bulk(op: 'move' | 'delete', paths: readonly string[], options: BulkOptions = {}): Promise<BulkResult> {
    const unique = [...new Set(paths)].sort();
    const covered = (p: string) => unique.some((q) => q !== p && (q === '/' || p.startsWith(`${q}/`)));
    if (op === 'move' && !options.to) throw new InvalidEdit('a bulk move needs a destination folder');
    const to = op === 'move' ? `/${(options.to ?? '').split('/').filter(Boolean).join('/')}` : null;
    const done: string[] = [];
    const skipped: string[] = [];
    await this.transaction(async () => {
      for (const p of unique) {
        const name = p.slice(p.lastIndexOf('/') + 1);
        const target = to === null ? null : `${to === '/' ? '' : to}/${name}`;
        if (covered(p) || target === p) {
          skipped.push(p);
          continue;
        }
        try {
          if (target === null) await this.remove(p, options);
          else await this.move(p, target, options);
        } catch (error) {
          if (error instanceof Error) error.message = `${p}: ${error.message}`;
          throw error;
        }
        done.push(p);
      }
    });
    return { op, to, done, skipped };
  }

  async importBatch(files: readonly ImportFile[], options: AuthorOptions = {}): Promise<ImportStats> {
    const stats: ImportStats = { created: 0, updated: 0, unchanged: 0, failed: 0, failures: [] };
    await this.transaction(async () => {
      for (const file of files) {
        // Its own savepoint: one refused path must not take the whole batch with it.
        await this.value('SAVEPOINT one_file');
        try {
          const result = await this.write(file.path, file.content, { author: options.author, message: 'import' });
          await this.value('RELEASE SAVEPOINT one_file');
          if (result.kind === 'noop') stats.unchanged++;
          else if (result.version === 1) stats.created++;
          else stats.updated++;
        } catch (error) {
          await this.value('ROLLBACK TO SAVEPOINT one_file');
          if (!(error instanceof TextdbError)) throw error;
          stats.failed++;
          stats.failures.push({ path: file.path, code: error.code, message: error.message });
        }
      }
    });
    return stats;
  }

  async setting(key: string): Promise<string | null> {
    return (await this.value<string | null>('SELECT kb.setting($1)', [key])) ?? null;
  }

  async setSetting(key: string, value: string | null): Promise<string | null> {
    return (await this.value<string | null>('SELECT kb.set_setting($1, $2)', [key, value])) ?? null;
  }

  /**
   * What `textdb sync` recorded for this folder and directory. `kb.sync` and `kb.sync_file` are
   * the CLI's own bookkeeping, with no function over them on either engine.
   */
  async syncState(prefix: string, dir: string): Promise<SyncState | null> {
    const row = await this.one<{
      id: number;
      dir: string;
      seq: number;
      synced_at: string;
      author: string | null;
      git_commit: string | null;
      git_branch: string | null;
      git_remote: string | null;
      git_clean: boolean | null;
    }>(
      `SELECT id, dir, seq, ${utc('synced_at')} AS synced_at, author, git_commit, git_branch, git_remote, git_clean
         FROM kb.sync WHERE prefix = $1 AND dir = $2`,
      [prefix, dir],
    );
    if (!row) return null;
    const changed = await this.value<number>('SELECT count(*) FROM kb.feed($1, 1000000)', [row.seq]);
    const conflicts = await this.rows<{ rel: string }>('SELECT rel FROM kb.sync_file WHERE sync_id = $1 AND conflict ORDER BY rel', [
      row.id,
    ]);
    return {
      dir: row.dir,
      seq: Number(row.seq),
      synced_at: row.synced_at,
      author: row.author,
      // One object, as the SQLite side gives it: null when the directory is in no checkout.
      git:
        row.git_commit === null && row.git_branch === null && row.git_remote === null
          ? null
          : { commit: row.git_commit, branch: row.git_branch, remote: row.git_remote, clean: row.git_clean === true },
      changed: Number(changed ?? 0),
      conflicts: conflicts.map((c) => c.rel),
    };
  }

  private unsupported(what: string): never {
    throw new Unsupported(`${what} is a SQLite store's, and this store is Postgres`);
  }

  trash(_parent?: number): Promise<TrashEntry[]> {
    this.unsupported('the trash');
  }

  trashEntry(_id: number): Promise<TrashEntry> {
    this.unsupported('the trash');
  }

  trashRead(_id: number, _version?: number): Promise<string> {
    this.unsupported('the trash');
  }

  trashHistory(_id: number): Promise<HistoryEntry[]> {
    this.unsupported('the trash');
  }

  purge(_id: number, _options?: AuthorOptions): Promise<PurgeStats> {
    this.unsupported('purging the trash');
  }

  emptyTrash(_options?: AuthorOptions): Promise<PurgeStats> {
    this.unsupported('emptying the trash');
  }

  /** Runs `fn` in one transaction, rolling back if it throws. */
  async transaction<T>(fn: () => Promise<T>): Promise<T> {
    await this.value('BEGIN');
    try {
      const result = await fn();
      await this.value('COMMIT');
      return result;
    } catch (error) {
      try {
        await this.value('ROLLBACK');
      } catch {
        // The connection may be gone; the original error is the one worth raising.
      }
      throw error;
    }
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await this.client.end();
  }
}

/** A LIKE pattern: a glob when `q` has `*` or `?`, otherwise "contains". Mirrors the SQLite side. */
function namePattern(q: string): string {
  const escaped = q.replace(/[\\%_]/g, (c) => `\\${c}`);
  return /[*?]/.test(q) ? escaped.replace(/\*/g, '%').replace(/\?/g, '_') : `%${escaped}%`;
}

/** `kb.write` and friends answer `{version, kind}` as jsonb. */
function asWrite(json: { version: number; kind: WriteResult['kind'] } | undefined): WriteResult {
  if (!json) throw new TextdbError('the write said nothing');
  const parsed = typeof json === 'string' ? (JSON.parse(json) as { version: number; kind: WriteResult['kind'] }) : json;
  return { version: Number(parsed.version), kind: parsed.kind };
}

/**
 * Polls `kb.feed` on its own corpus and emits rows in seq order.
 *
 * The extension notifies `textdb_change` in the transaction of each change, which is what the
 * CLI's `watch` waits on; polling is what this does instead, because one client here may be
 * serving many requests and a connection parked in `LISTEN` is a connection not answering them.
 */
class PgWatcher implements ChangeWatcher {
  private readonly timer: NodeJS.Timeout;
  private seq = -1;
  private closed = false;
  private running = false;

  private readonly corpus: PgCorpus;
  private readonly options: WatchOptions;

  constructor(corpus: PgCorpus, options: WatchOptions) {
    this.corpus = corpus;
    this.options = options;
    this.timer = setInterval(() => void this.tick(), Math.max(50, options.intervalMs ?? 500));
    if (options.since !== undefined) this.seq = options.since;
  }

  private async tick(): Promise<void> {
    if (this.closed || this.running) return;
    this.running = true;
    try {
      if (this.seq < 0) this.seq = await this.corpus.lastSeq();
      const rows = await this.corpus.feed(this.seq, this.options.batchSize ?? 500);
      for (const change of rows) {
        if (this.closed) return;
        this.seq = Math.max(this.seq, change.seq);
        this.options.onChange(change);
      }
    } catch (error) {
      this.options.onError?.(error instanceof TextdbError ? error : new TextdbError(String(error)));
    } finally {
      this.running = false;
    }
  }

  close(): void {
    this.closed = true;
    clearInterval(this.timer);
  }
}
