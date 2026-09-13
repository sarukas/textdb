import { type Change, type Corpus, type Hunk, NotFound, type Watcher } from '@textdb/node';

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
  private readonly corpus: Corpus;
  private readonly watcher: Watcher;
  private readonly subscriptions = new Set<Subscription>();
  private readonly hunkCache = new Map<number, Hunk[] | null>();

  constructor(corpus: Corpus, options: { intervalMs?: number } = {}) {
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
      let last = since ?? this.corpus.lastSeq();
      for (;;) {
        const page = this.corpus.feed(last, BACKLOG_PAGE);
        for (const change of page) {
          if (signal.aborted) return;
          last = change.seq;
          yield this.withHunks(change);
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

  close(): void {
    this.watcher.close();
    for (const subscription of this.subscriptions) subscription.close();
  }

  private publish(change: Change): void {
    if (this.subscriptions.size === 0) return;
    const event = this.withHunks(change);
    for (const subscription of this.subscriptions) subscription.push(event);
  }

  private withHunks(change: Change): ChangeEvent {
    if (change.op !== 'commit' || change.version === null) return change;
    let hunks = this.hunkCache.get(change.seq);
    if (hunks === undefined) {
      hunks = this.loadHunks(change.path, change.version);
      this.hunkCache.set(change.seq, hunks);
      if (this.hunkCache.size > HUNK_CACHE_SIZE) this.hunkCache.delete(this.hunkCache.keys().next().value!);
    }
    return hunks ? { ...change, hunks } : change;
  }

  /** Null when the hunks are too large to inline or the file has since moved away. */
  private loadHunks(path: string, version: number): Hunk[] | null {
    try {
      const hunks = this.corpus.hunks(path, version - 1, version);
      let size = 0;
      for (const h of hunks) size += Buffer.byteLength(h.old_text) + Buffer.byteLength(h.new_text);
      return size <= HUNK_TEXT_LIMIT ? hunks : null;
    } catch (error) {
      if (error instanceof NotFound) return null;
      throw error;
    }
  }
}
