export { resolveExtension } from './connection.ts';
export {
  type AuthorOptions,
  type BulkOptions,
  type BulkResult,
  Corpus,
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
  InvalidEdit,
  NotFound,
  TextdbError,
  fromMessage,
  toTextdbError,
} from './errors.ts';
export type * from './types.ts';
export { SORT_KEYS } from './types.ts';
export { type WatchOptions, Watcher } from './watch.ts';
