import { type Change, type ChangeWatcher, type CorpusApi, type Hunk, NotFound } from '@textdb/node';

export type ChangeEvent = Change & { hunks?: Hunk[] };

const HUNK_TEXT_LIMIT = 256 * 1024;
const HUNK_CACHE_SIZE = 1000;
const BACKLOG_PAGE = 500;
/** A client this far behind is dropped; it reconnects with Last-Event-ID. */
const MAX_QUEUE = 10_000;

class Subscription {
  private readonly queue: ChangeEvent[] = [];
  private wake: (() => void) | undefined;
  private closed = false;

  push(event: ChangeEvent): void {
    if (this.closed) return;
    if (this.queue.length >= MAX_QUEUE) return this.close();
    this.queue.push(event);
    this.notify();
  }

  /** The next event, or undefined once closed. */
  async next(): Promise<ChangeEvent | undefined> {
    while (!this.closed) {
      const event = this.queue.shift();
      if (event) return event;
      await new Promise<void>((resolve) => {
        this.wake = resolve;
      });
    }
    return undefined;
  }

  close(): void {
    this.closed = true;
    this.notify();
  }

  private notify(): void {
    const wake = this.wake;
    this.wake = undefined;
    wake?.();
  }
}

/** One watcher on the store, fanned out to every event-stream subscriber. */
export class ChangeHub {
  private readonly corpus: CorpusApi;
  private readonly watcher: ChangeWatcher;
  private readonly subscriptions = new Set<Subscription>();
  private readonly hunkCache = new Map<number, Hunk[] | null>();

  constructor(corpus: CorpusApi, options: { intervalMs?: number } = {}) {
    this.corpus = corpus;
    this.watcher = corpus.watch({
      intervalMs: options.intervalMs,
      onChange: (change) => this.publish(change),
      onError: (error) => console.error('change feed:', error),
    });
  }

  get subscriberCount(): number {
    return this.subscriptions.size;
  }

  /**
   * Changes after `since` (default: the current last seq), backlog first, then live, until
   * `signal` aborts. The subscription is registered before the backlog is read, so a change
   * committed meanwhile arrives live; seq order drops what the backlog already sent.
   */
  async *changes(since: number | undefined, signal: AbortSignal): AsyncGenerator<ChangeEvent> {
    const subscription = new Subscription();
    this.subscriptions.add(subscription);
    const stop = () => subscription.close();
    signal.addEventListener('abort', stop, { once: true });
    try {
      let last = since ?? (await this.corpus.lastSeq());
      for (;;) {
        const page = await this.corpus.feed(last, BACKLOG_PAGE);
        for (const change of page) {
          if (signal.aborted) return;
          last = change.seq;
          yield await this.withHunks(change);
        }
        if (page.length < BACKLOG_PAGE) break;
      }
      for (let event = await subscription.next(); event; event = await subscription.next()) {
        if (event.seq <= last) continue;
        last = event.seq;
        yield event;
      }
    } finally {
      signal.removeEventListener('abort', stop);
      subscription.close();
      this.subscriptions.delete(subscription);
    }
  }

  /** Publishing is chained through this, so subscribers see seq order. */
  private pending: Promise<void> = Promise.resolve();

  close(): void {
    this.watcher.close();
    for (const subscription of this.subscriptions) subscription.close();
  }

  /**
   * Enriching a change means reading its hunks, which a Postgres store answers asynchronously, so
   * this queues rather than blocks -- chained, so subscribers still see changes in seq order.
   */
  private publish(change: Change): void {
    if (this.subscriptions.size === 0) return;
    this.pending = this.pending
      .then(async () => {
        const event = await this.withHunks(change);
        for (const subscription of this.subscriptions) subscription.push(event);
      })
      .catch(() => {
        // A change that cannot be enriched is not a reason to stop the feed.
      });
  }

  private async withHunks(change: Change): Promise<ChangeEvent> {
    if (change.op !== 'commit' || change.version === null) return change;
    let hunks = this.hunkCache.get(change.seq);
    if (hunks === undefined) {
      hunks = await this.loadHunks(change.path, change.version);
      this.hunkCache.set(change.seq, hunks);
      if (this.hunkCache.size > HUNK_CACHE_SIZE) this.hunkCache.delete(this.hunkCache.keys().next().value!);
    }
    return hunks ? { ...change, hunks } : change;
  }

  /** Null when the hunks are too large to inline or the file has since moved away. */
  private async loadHunks(path: string, version: number): Promise<Hunk[] | null> {
    try {
      const hunks = await this.corpus.hunks(path, version - 1, version);
      let size = 0;
      for (const h of hunks) size += Buffer.byteLength(h.old_text) + Buffer.byteLength(h.new_text);
      return size <= HUNK_TEXT_LIMIT ? hunks : null;
    } catch (error) {
      if (error instanceof NotFound) return null;
      throw error;
    }
  }
}
