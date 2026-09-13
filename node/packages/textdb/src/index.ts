export { resolveExtension } from './connection.ts';
export { type AuthorOptions, Corpus, type OpenOptions, type Stat, type WriteOptions, openCorpus } from './corpus.ts';
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
export { type WatchOptions, Watcher } from './watch.ts';
