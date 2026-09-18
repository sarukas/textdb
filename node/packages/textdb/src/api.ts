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
import type {
  Change,
  Chunk,
  Entry,
  FileView,
  HeadingName,
  HistoryEntry,
  Hunk,
  Info,
  Link,
  LinkOptions,
  ListOptions,
  ListPage,
  OutlineEntry,
  PropertyHit,
  PropertyKey,
  PropertyValue,
  SearchHit,
  Whoami,
  WriteResult,
} from './types.ts';
import type { WatchOptions } from './watch.ts';

/**
 * A value, or a promise of one.
 *
 * The reason this interface is shaped in terms of it: SQLite is reached through `node:sqlite`,
 * which is synchronous, and no Postgres driver for Node is. Declaring the surface as
 * `Awaitable<T>` lets one interface describe both, and lets the synchronous [`Corpus`] satisfy it
 * with no wrapper at all -- a caller writes `await api.ls('/')` and neither knows nor cares.
 */
export type Awaitable<T> = T | Promise<T>;

/** Which engine a corpus is talking to. */
export type Backend = 'sqlite' | 'postgres';

/**
 * A running subscription to the change feed: all a caller needs of one is the ability to stop it.
 * SQLite's `Watcher` polls `PRAGMA data_version` on a connection of its own; Postgres polls
 * `kb.feed`. Both are this.
 */
export interface ChangeWatcher {
  close(): void;
}

/**
 * What this backend can do, for a client that must not offer what would fail.
 *
 * Trash and `revert-batch` are SQLite's alone, as `textdb --help` says of the commands: the
 * shadow tables they read are part of the SQLite schema and the Postgres extension has no
 * equivalent. A UI that shows a Trash view against a central store would be offering a button
 * that errors, so the answer belongs in the API rather than in a comment.
 */
export interface Capabilities {
  /** `trash`, `trashEntry`, `trashRead`, `trashHistory`, `purge`, `emptyTrash`. */
  trash: boolean;
  /** Undoing one `sql --write` batch by its id. */
  revertBatch: boolean;
  /** `syncState`: what a directory and a folder last agreed on. */
  syncState: boolean;
}

/**
 * Every operation a corpus answers, in either backend.
 *
 * The synchronous [`Corpus`] (SQLite) and `PgCorpus` (Postgres) both satisfy this. Where a
 * backend cannot do something at all, it throws `Unsupported` and says so in `capabilities`
 * rather than pretending: see [`Capabilities`].
 */
export interface CorpusApi {
  readonly backend: Backend;
  /** What the store is: a file path for SQLite, a URL with its password removed for Postgres. */
  readonly db: string;
  /** The account this corpus authenticated as, or null for the owner. */
  readonly account: string | null;
  readonly capabilities: Capabilities;

  authenticate(bearer: string | null): Awaitable<string | null>;
  whoami(): Awaitable<Whoami>;

  info(): Awaitable<Info>;
  lastSeq(): Awaitable<number>;
  feed(since: number, limit?: number): Awaitable<Change[]>;
  watch(options: WatchOptions): ChangeWatcher;

  ls(dir?: string): Awaitable<Entry[]>;
  list(dir?: string, options?: ListOptions): Awaitable<ListPage>;
  entry(target: string): Awaitable<Entry>;
  stat(target: string): Awaitable<Stat>;
  read(filePath: string, version?: number): Awaitable<FileView>;
  readBytes(filePath: string): Awaitable<Uint8Array>;
  chunks(filePath: string, version?: number): Awaitable<Chunk[]>;
  history(filePath: string): Awaitable<HistoryEntry[]>;
  hunks(filePath: string, from?: number, to?: number): Awaitable<Hunk[]>;
  diff(filePath: string, from: number, to: number): Awaitable<string>;
  pathHistory(filePath: string): Awaitable<PathEvent[]>;
  pathHistoryOf(id: number): Awaitable<PathEvent[]>;
  exportFiles(dir?: string): Awaitable<ExportFile[]>;

  search(query: string, options?: { prefix?: string; limit?: number; perFile?: number }): Awaitable<SearchHit[]>;
  links(path?: string, options?: LinkOptions): Awaitable<Link[]>;
  backlinks(path?: string, options?: LinkOptions): Awaitable<Link[]>;
  outline(
    path?: string,
    options?: { heading?: string; match?: 'exact' | 'prefix' | 'contains'; level?: number; limit?: number },
  ): Awaitable<OutlineEntry[]>;
  headingNames(path?: string, options?: { starts?: string; limit?: number }): Awaitable<HeadingName[]>;
  propertyKeys(options?: { prefix?: string; limit?: number }): Awaitable<PropertyKey[]>;
  propertyValues(key: string, options?: { prefix?: string; limit?: number }): Awaitable<PropertyValue[]>;
  propertyFind(query: string, options?: { folder?: string; limit?: number }): Awaitable<PropertyHit[]>;

  write(filePath: string, content: string, options?: WriteOptions): Awaitable<WriteResult>;
  replaceLines(
    filePath: string,
    from: number,
    to: number,
    text: string,
    options?: Omit<WriteOptions, 'message'>,
  ): Awaitable<WriteResult>;
  edit(filePath: string, oldText: string, newText: string, options?: AuthorOptions): Awaitable<number>;
  append(filePath: string, text: string, options?: AuthorOptions): Awaitable<number>;
  move(from: string, to: string, options?: AuthorOptions): Awaitable<void>;
  remove(target: string, options?: AuthorOptions): Awaitable<void>;
  bulk(op: 'move' | 'delete', paths: readonly string[], options?: BulkOptions): Awaitable<BulkResult>;
  importBatch(files: readonly ImportFile[], options?: AuthorOptions): Awaitable<ImportStats>;

  setting(key: string): Awaitable<string | null>;
  setSetting(key: string, value: string | null): Awaitable<string | null>;

  /** SQLite only; see [`Capabilities`]. */
  syncState(prefix: string, dir: string): Awaitable<SyncState | null>;
  trash(parent?: number): Awaitable<TrashEntry[]>;
  trashEntry(id: number): Awaitable<TrashEntry>;
  trashRead(id: number, version?: number): Awaitable<string>;
  trashHistory(id: number): Awaitable<HistoryEntry[]>;
  purge(id: number, options?: AuthorOptions): Awaitable<PurgeStats>;
  emptyTrash(options?: AuthorOptions): Awaitable<PurgeStats>;

  close(): Awaitable<void>;
}
