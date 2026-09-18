export type { Awaitable, Backend, Capabilities, ChangeWatcher, CorpusApi } from './api.ts';
export { resolveExtension } from './connection.ts';
export { connect, parseStore, type ConnectOptions, type StoreUrl } from './connect.ts';
export { PgCorpus, redactUrl } from './pg.ts';
export {
  type AuthorOptions,
  type BulkOptions,
  type BulkResult,
  Corpus,
  type ExportFile,
  type SyncState,
  type OpenOptions,
  type PathEvent,
  type PurgeStats,
  type Stat,
  type TrashEntry,
  type WriteOptions,
  openCorpus,
} from './corpus.ts';
export {
  Conflict,
  type ConflictPayload,
  Contention,
  type ErrorCode,
  Forbidden,
  InvalidEdit,
  NotFound,
  Unsupported,
  TextdbError,
  fromMessage,
  toTextdbError,
} from './errors.ts';
export type * from './types.ts';
export { LINK_STATUSES, SORT_KEYS } from './types.ts';
export { type WatchOptions, Watcher } from './watch.ts';
