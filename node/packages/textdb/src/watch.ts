import type { DatabaseSync } from 'node:sqlite';
import { openConnection } from './connection.ts';
import { type TextdbError, toTextdbError } from './errors.ts';
import { Sql } from './sql.ts';
import type { Change } from './types.ts';

export interface WatchOptions {
  /** Emit changes with `seq > since`; defaults to the store's last seq when the watcher starts. */
  since?: number;
  intervalMs?: number;
  /** Feed rows read per query while catching up. */
  batchSize?: number;
  onChange: (change: Change) => void;
  onError?: (error: TextdbError) => void;
}

/**
 * Polls `PRAGMA data_version` on a connection of its own and emits feed rows in seq order.
 * `data_version` only moves for commits made by *other* connections, so sharing the writer's
 * connection would hide that writer's own commits.
 */
export class Watcher {
  private readonly sql: Sql;
  private readonly timer: NodeJS.Timeout;
  private readonly options: WatchOptions;
  private seq: number;
  private dataVersion: number | undefined;
  private closed = false;

  constructor(dbPath: string, extension: string, options: WatchOptions) {
    this.options = options;
    const conn: DatabaseSync = openConnection(dbPath, extension);
    this.sql = new Sql(conn);
    if (options.since === undefined) {
      // Read the version before the seq: a commit landing in between then still moves it.
      this.dataVersion = this.readDataVersion();
      this.seq = Number(this.sql.value('SELECT textdb_last_seq()'));
    } else {
      this.seq = options.since;
    }
    this.timer = setInterval(() => this.poll(), options.intervalMs ?? 100);
  }

  /** The seq of the last change emitted (or the starting point). */
  get lastSeq(): number {
    return this.seq;
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    clearInterval(this.timer);
    this.sql.conn.close();
  }

  private poll(): void {
    const batchSize = this.options.batchSize ?? 1000;
    try {
      const dataVersion = this.readDataVersion();
      if (dataVersion === this.dataVersion) return;
      for (;;) {
        const rows = this.sql.all<Change>('SELECT * FROM textdb_feed(?, ?)', this.seq, batchSize);
        for (const row of rows) {
          this.seq = row.seq;
          this.options.onChange(row);
          if (this.closed) return;
        }
        if (rows.length < batchSize) break;
      }
      // Only once fully drained, so a failed read is retried on the next tick.
      this.dataVersion = dataVersion;
    } catch (error) {
      const err = toTextdbError(error);
      if (this.options.onError) this.options.onError(err);
      else console.error('textdb watcher:', err);
    }
  }

  private readDataVersion(): number {
    return Number(this.sql.value('PRAGMA data_version'));
  }
}
