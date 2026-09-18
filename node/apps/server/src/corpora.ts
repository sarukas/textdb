import { createHash } from 'node:crypto';
import { type CorpusApi, connect } from '@textdb/node';

/**
 * The corpora this server answers through: the owner's, and one per bearer.
 *
 * A bearer belongs to a connection -- `textdb_auth` on SQLite binds the view to the connection it
 * was called on, and Postgres holds it in a session GUC -- so one corpus is one account. Serving
 * several accounts therefore means one corpus each, never re-authenticating a shared one between
 * requests: two requests interleaving on one connection would answer in each other's view, which
 * is the one failure this whole feature exists to prevent.
 *
 * Keyed by a hash of the bearer, so the map holds no token in plain text; the corpus itself holds
 * the connection that authenticated with it. Idle corpora are closed, which also bounds how long a
 * revoked token keeps a warm connection: a revoked bearer is refused at the next open, and the
 * store re-checks nothing in between.
 */
export class Corpora {
  private readonly store: string;
  private readonly extension: string | undefined;
  private readonly idleMs: number;
  private readonly max: number;
  private readonly accounts = new Map<string, Entry>();
  private ownerCorpus: Promise<CorpusApi> | undefined;
  private sweeper: NodeJS.Timeout | undefined;
  private closed = false;

  constructor(options: { store: string; extension?: string | undefined; idleMs?: number; max?: number }) {
    this.store = options.store;
    this.extension = options.extension;
    this.idleMs = options.idleMs ?? 5 * 60_000;
    this.max = options.max ?? 64;
  }

  /** The owner's corpus: one, shared, and open for the life of the server. */
  owner(): Promise<CorpusApi> {
    if (this.closed) throw new Error('this server is shutting down');
    this.ownerCorpus ??= connect({ store: this.store, extension: this.extension });
    return this.ownerCorpus;
  }

  /**
   * The corpus for this bearer, opened on first use.
   *
   * An unusable bearer throws `Forbidden` from the store, which the error layer turns into a 403.
   * Nothing is cached for it, so a token that starts working later needs no restart.
   */
  async forToken(bearer: string): Promise<CorpusApi> {
    if (this.closed) throw new Error('this server is shutting down');
    const key = createHash('sha256').update(bearer).digest('hex');
    const held = this.accounts.get(key);
    if (held) {
      held.used = Date.now();
      return held.corpus;
    }
    const opening = connect({ store: this.store, extension: this.extension, token: bearer });
    const entry: Entry = { corpus: opening, used: Date.now() };
    this.accounts.set(key, entry);
    this.start();
    try {
      await opening;
    } catch (error) {
      // A refused bearer is not a corpus: forget it rather than answering every later request
      // from a rejected promise.
      this.accounts.delete(key);
      throw error;
    }
    await this.evictOverflow();
    return opening;
  }

  /** How many account corpora are open, for a status page or a test. */
  get open(): number {
    return this.accounts.size;
  }

  private start(): void {
    if (this.sweeper || this.closed) return;
    this.sweeper = setInterval(() => void this.sweep(), Math.max(1000, Math.floor(this.idleMs / 2)));
    // A sweeper must not be the reason the process stays up.
    this.sweeper.unref?.();
  }

  private async sweep(): Promise<void> {
    const cutoff = Date.now() - this.idleMs;
    for (const [key, entry] of [...this.accounts]) {
      if (entry.used <= cutoff) await this.drop(key, entry);
    }
  }

  /** Over the cap, the least recently used goes first. */
  private async evictOverflow(): Promise<void> {
    while (this.accounts.size > this.max) {
      const oldest = [...this.accounts].sort((a, b) => a[1].used - b[1].used)[0];
      if (!oldest) return;
      await this.drop(oldest[0], oldest[1]);
    }
  }

  private async drop(key: string, entry: Entry): Promise<void> {
    this.accounts.delete(key);
    try {
      await (await entry.corpus).close();
    } catch {
      // Closing a connection that is already gone is not worth reporting.
    }
  }

  async close(): Promise<void> {
    this.closed = true;
    if (this.sweeper) clearInterval(this.sweeper);
    for (const [key, entry] of [...this.accounts]) await this.drop(key, entry);
    if (this.ownerCorpus) {
      const corpus = await this.ownerCorpus.catch(() => undefined);
      await corpus?.close();
    }
  }
}

interface Entry {
  corpus: Promise<CorpusApi>;
  /** When it last served a request, for the idle sweep and the cap. */
  used: number;
}
