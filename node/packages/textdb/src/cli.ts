import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { type ErrorCode, TextdbError, errorOf } from './errors.ts';

/**
 * The textdb CLI, run for the things that are not a corpus operation.
 *
 * Accounts, tokens, shares and asset stores are configuration, and the rule for each lives in the
 * store -- `admin_only` in the SQLite store module, a trigger and `kb.*` on Postgres. Running the
 * command that already knows how to ask means a refusal comes from where the rule is, rather than
 * being re-implemented here and drifting from it. It also means one implementation for both
 * engines: a central store on Postgres is configured exactly like a local one.
 *
 * Everything a caller supplies is one argument, `--flag=value`, so a name or a token beginning with
 * a dash is a name or a token and not a flag.
 */

/** The names the CLI would otherwise take from the environment; a caller's arguments decide. */
const FROM_ARGUMENTS = ['TEXTDB_STORE', 'TEXTDB_AUTHOR', 'TEXTDB_PATH_HISTORY'];

/**
 * The CLI to run: the one named, else a build in this checkout's `target/`.
 *
 * Walks up from this module, so it finds the build whether the SDK is used from the repository, from
 * an app inside it, or from `node_modules` in one.
 */
export function findCli(explicit?: string | null): string | null {
  if (explicit) return existsSync(explicit) ? explicit : null;
  const exe = process.platform === 'win32' ? 'textdb.exe' : 'textdb';
  let dir = fileURLToPath(new URL('.', import.meta.url));
  for (;;) {
    for (const profile of ['release', 'debug']) {
      const candidate = join(dir, 'target', profile, exe);
      if (existsSync(candidate)) return candidate;
    }
    const up = dirname(dir);
    if (up === dir) return null;
    dir = up;
  }
}

/** One run, with its exit status: a non-zero status is an answer here, not a throw. */
export function runCli(cli: string, args: string[]): Promise<{ stdout: string; stderr: string; status: number }> {
  const env = { ...process.env };
  for (const name of FROM_ARGUMENTS) delete env[name];
  return new Promise((resolve, reject) => {
    execFile(cli, args, { env, maxBuffer: 256 * 1024 * 1024, windowsHide: true }, (error, stdout, stderr) => {
      const code = (error as { code?: unknown } | null)?.code;
      if (error && typeof code !== 'number') return reject(error);
      resolve({ stdout, stderr, status: typeof code === 'number' ? code : 0 });
    });
  });
}

export interface CliOptions {
  /** The store, as `textdb --store` takes it: a path, or a Postgres URL. */
  store: string;
  /** The CLI to run; found in this checkout when left out. */
  cli?: string | null;
  /** Presented as the session's bearer, so the store answers in that account's name. */
  token?: string;
  /** Recorded as the author of anything this writes. */
  author?: string;
}

/** The JSON objects a run printed, one per line, on either stream. */
function jsonLines(text: string): Record<string, unknown>[] {
  const out: Record<string, unknown>[] = [];
  for (const line of text.split(/\r?\n/)) {
    const t = line.trim();
    if (!t.startsWith('{')) continue;
    try {
      const v: unknown = JSON.parse(t);
      if (v && typeof v === 'object' && !Array.isArray(v)) out.push(v as Record<string, unknown>);
    } catch {
      // A line of prose, not an answer.
    }
  }
  return out;
}

function asArray(text: string): unknown[] | undefined {
  const t = text.trim();
  if (!t.startsWith('[')) return undefined;
  try {
    const v: unknown = JSON.parse(t);
    return Array.isArray(v) ? v : undefined;
  } catch {
    return undefined;
  }
}

/**
 * A CLI bound to one store, answering JSON.
 *
 * A refusal arrives as `{"error": {"code", "message"}}` and is thrown as the error class that code
 * means -- `Forbidden` for TX005, `NotFound` for TX003 -- so a caller handles a CLI refusal exactly
 * as it handles one from a corpus call. A code this SDK does not know is a `TextdbError`, never a
 * silent success.
 */
export class Cli {
  private readonly store: string;
  private readonly cliPath: string | null;
  private readonly token: string | undefined;
  private readonly author: string | undefined;

  constructor(options: CliOptions) {
    this.store = options.store;
    this.cliPath = options.cli === undefined ? findCli() : options.cli;
    this.token = options.token;
    this.author = options.author;
  }

  /** Whether a CLI was found: without one, none of these commands can be run at all. */
  get available(): boolean {
    return this.cliPath !== null;
  }

  get path(): string | null {
    return this.cliPath;
  }

  /** The same CLI and store, as somebody else: one corpus, one session, as everywhere in textdb. */
  as(options: { token?: string; author?: string }): Cli {
    return new Cli({ store: this.store, cli: this.cliPath, token: options.token ?? this.token, author: options.author ?? this.author });
  }

  /**
   * Run `args` and return what it printed.
   *
   * An answer is a JSON object or array on either stream: the CLI prints notes on stderr, so the
   * one that parses is the answer. A non-zero status with no error object of its own still throws,
   * because a command that failed has not answered.
   */
  async json<T>(args: string[]): Promise<T> {
    if (!this.cliPath) {
      throw errorOf('TX004', 'the textdb CLI was not found: build it (cargo build --release -p textdb-cli) or name it with TEXTDB_CLI');
    }
    const global = ['--store', this.store, '--json'];
    if (this.token) global.push(`--token=${nul('the token', this.token)}`);
    if (this.author) global.push(`--author=${nul('the author', this.author)}`);
    const { stdout, stderr, status } = await runCli(this.cliPath, [...global, ...args]);
    const printed = [...jsonLines(stdout), ...jsonLines(stderr)];
    const failed = printed.find((o) => typeof o.error === 'object' && o.error !== null)?.error as Record<string, unknown> | undefined;
    if (failed) {
      const code = typeof failed.code === 'string' && /^TX00[0-5]$/.test(failed.code) ? (failed.code as ErrorCode) : 'TX000';
      throw errorOf(code, typeof failed.message === 'string' ? failed.message : `textdb ${args.join(' ')} was refused`);
    }
    if (status === 0) {
      const array = [stdout, stderr].map(asArray).find((a) => a !== undefined);
      if (array) return array as T;
      const object = printed[0];
      if (object) return object as T;
      // A command that answers nothing at all (a successful `account disable`) answers nothing.
      return undefined as T;
    }
    throw new TextdbError((stderr || stdout).trim().slice(0, 2000) || `textdb ${args.join(' ')} exited with status ${status}`);
  }
}

/** No NUL: no command line can carry one, and Node throws its own TypeError over it. */
function nul(what: string, value: string): string {
  if (value.includes('\x00')) throw errorOf('TX004', `${what} must not contain a NUL character`);
  return value;
}
