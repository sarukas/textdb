import type { DatabaseSync, SQLInputValue, StatementSync } from 'node:sqlite';
import { toTextdbError } from './errors.ts';

const decoder = new TextDecoder();

/** Prepared-statement cache over one connection; every SQLite error leaves as a `TextdbError`. */
export class Sql {
  readonly conn: DatabaseSync;
  private readonly statements = new Map<string, StatementSync>();

  constructor(conn: DatabaseSync) {
    this.conn = conn;
  }

  all<T>(sql: string, ...params: SQLInputValue[]): T[] {
    return guard(() => this.prepare(sql).all(...bind(params)).map((row) => ({ ...row }) as T));
  }

  get<T>(sql: string, ...params: SQLInputValue[]): T | undefined {
    return guard(() => {
      const row = this.prepare(sql).get(...bind(params));
      return row === undefined ? undefined : ({ ...row } as T);
    });
  }

  /** The first column of the first row. */
  value(sql: string, ...params: SQLInputValue[]): unknown {
    return guard(() => {
      const row = this.prepare(sql).get(...bind(params));
      return row === undefined ? undefined : Object.values(row)[0];
    });
  }

  /** Runs a statement and returns the number of rows it changed. */
  run(sql: string, ...params: SQLInputValue[]): number {
    return guard(() => Number(this.prepare(sql).run(...bind(params)).changes));
  }

  exec(sql: string): void {
    guard(() => this.conn.exec(sql));
  }

  private prepare(sql: string): StatementSync {
    let statement = this.statements.get(sql);
    if (!statement) {
      statement = this.conn.prepare(sql);
      this.statements.set(sql, statement);
    }
    return statement;
  }
}

/** node:sqlite binds every JS number as REAL, and the textdb functions reject a REAL version or line. */
function bind(params: SQLInputValue[]): SQLInputValue[] {
  return params.map((p) => (typeof p === 'number' && Number.isInteger(p) ? BigInt(p) : p));
}

function guard<T>(fn: () => T): T {
  try {
    return fn();
  } catch (error) {
    throw toTextdbError(error);
  }
}

/** Content arrives as TEXT, or as a BLOB when it is not valid UTF-8. */
export function asText(value: unknown): string {
  if (typeof value === 'string') return value;
  if (value instanceof Uint8Array) return decoder.decode(value);
  return '';
}

/** `?, ?, ?` for the arguments that are present, so optional SQL arguments fall back to their defaults. */
export function placeholders(args: readonly unknown[]): string {
  return args.map(() => '?').join(', ');
}
