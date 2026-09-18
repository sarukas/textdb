import type { CorpusApi } from './api.ts';
import { type Corpus, openCorpus } from './corpus.ts';
import { InvalidEdit } from './errors.ts';
import { PgCorpus } from './pg.ts';

/** Which engine a store string names, and what to hand the driver. */
export type StoreUrl = { backend: 'sqlite'; path: string } | { backend: 'postgres'; url: string };

/**
 * What a store string names, by the same rules the CLI's `--store` uses: a Postgres URL, or a
 * SQLite file. `sqlite:` is accepted and stripped, so one spelling works everywhere.
 *
 * Nothing here touches the disk or the network: a caller can ask what a string means without
 * opening anything.
 */
export function parseStore(store: string): StoreUrl {
  const trimmed = store.trim();
  if (/^postgres(ql)?:\/\//i.test(trimmed)) return { backend: 'postgres', url: trimmed };
  if (/^sqlite:/i.test(trimmed)) return { backend: 'sqlite', path: trimmed.slice(trimmed.indexOf(':') + 1) };
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed)) {
    throw new InvalidEdit(`${store}: a store is a SQLite file or a postgres:// URL`);
  }
  return { backend: 'sqlite', path: trimmed };
}

export interface ConnectOptions {
  /** A SQLite file (`kb.db`, `sqlite:kb.db`) or a Postgres URL (`postgres://user@host/db`). */
  store: string;
  /** Default author for writes that do not name one. */
  author?: string;
  /** A bearer token: every answer is then that account's view. */
  token?: string;
  /** SQLite only: the loadable extension; see `resolveExtension`. */
  extension?: string;
}

/**
 * Open a corpus over either engine.
 *
 * Async because connecting to Postgres is, and one door is better than two that drift. The
 * SQLite corpus it returns is the synchronous [`Corpus`] -- which satisfies [`CorpusApi`] as it
 * stands, since every method there is declared `Awaitable` -- so a local store costs no promises
 * it does not need, and a caller that awaits works against both.
 */
export async function connect(options: ConnectOptions): Promise<CorpusApi> {
  const parsed = parseStore(options.store);
  if (parsed.backend === 'postgres') {
    return PgCorpus.open({ url: parsed.url, author: options.author, token: options.token });
  }
  const corpus: Corpus = openCorpus({
    db: parsed.path,
    author: options.author,
    extension: options.extension,
    token: options.token,
  });
  return corpus;
}
