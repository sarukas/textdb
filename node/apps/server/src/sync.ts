import { execFile } from 'node:child_process';
import { existsSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { type CorpusApi, type ErrorCode, NotFound, type SyncState } from '@textdb/node';
import type { SyncLinkConfig } from './config.ts';
import { badRequest, CodedError } from './errors.ts';
import { resolveMarkers } from './markers.ts';

/** `--base` values: commit ids and ordinary ref names, nothing git could take for an option. */
const REV = /^[\w.~^@{}/+-]+$/;

export interface SyncLinkState extends SyncLinkConfig {
  exists: boolean;
  /** A sync of this folder is running. */
  running: boolean;
  last: SyncState | null;
}

export interface SyncLinks {
  available: boolean;
  /** Why syncing is unavailable. */
  reason: string | null;
  links: SyncLinkState[];
}

export interface RunOptions {
  dryRun?: boolean;
  commit?: boolean;
  base?: string | undefined;
  author?: string | undefined;
}

/** The textdb CLI: `TEXTDB_CLI`, else the repository's release or debug build. */
export function findCli(explicit: string | undefined): string | null {
  if (explicit) return existsSync(explicit) ? explicit : null;
  const exe = process.platform === 'win32' ? 'textdb.exe' : 'textdb';
  for (const profile of ['release', 'debug']) {
    // node/apps/server/src → the repository root.
    const candidate = fileURLToPath(new URL(`../../../../target/${profile}/${exe}`, import.meta.url));
    if (existsSync(candidate)) return candidate;
  }
  return null;
}

export function runCli(cli: string, args: string[]): Promise<{ stdout: string; stderr: string; status: number }> {
  const env = { ...process.env };
  // The store, author and settings come from the arguments, never from the server's environment.
  for (const name of ['TEXTDB_STORE', 'TEXTDB_AUTHOR', 'TEXTDB_PATH_HISTORY']) delete env[name];
  return new Promise((resolve, reject) => {
    execFile(cli, args, { env, maxBuffer: 256 * 1024 * 1024, windowsHide: true }, (error, stdout, stderr) => {
      const code = (error as { code?: unknown } | null)?.code;
      if (error && typeof code !== 'number') return reject(error);
      resolve({ stdout, stderr, status: typeof code === 'number' ? code : 0 });
    });
  });
}

/**
 * Syncs the folders listed in `TEXTDB_SYNC` with their directories by running `textdb sync`, so
 * the web UI gets exactly the CLI's behaviour: three-way merge, conflict markers, git authors and
 * commits. One sync per folder at a time.
 */
export class SyncService {
  readonly links: SyncLinkConfig[];
  private readonly corpus: CorpusApi;
  private readonly cli: string | null;
  private readonly running = new Set<string>();

  constructor(corpus: CorpusApi, links: SyncLinkConfig[], cli: string | null) {
    this.corpus = corpus;
    this.links = links;
    this.cli = cli;
  }

  /** The textdb CLI this server runs, when it was found. */
  get cliPath(): string | null {
    return this.cli;
  }

  /** Run `fn` as the only sync, pull or push of the folder `prefix` at the moment. */
  async exclusive<T>(prefix: string, fn: () => Promise<T>): Promise<T> {
    if (this.running.has(prefix)) throw new CodedError('TX002', `${prefix} is being synced already; try again when it finishes`);
    this.running.add(prefix);
    try {
      return await fn();
    } finally {
      this.running.delete(prefix);
    }
  }

  async list(): Promise<SyncLinks> {
    return {
      available: this.cli !== null,
      reason: this.cli ? null : 'The textdb CLI was not found: build it (cargo build --release -p textdb-cli) or set TEXTDB_CLI.',
      links: await Promise.all(
        this.links.map(async (link) => ({
          ...link,
          exists: existsSync(link.dir),
          running: this.running.has(link.prefix),
          last: await this.state(link),
        })),
      ),
    };
  }

  /** Why syncing is unavailable, without asking the store anything. */
  get unavailable(): string | null {
    return this.cli ? null : 'The textdb CLI was not found: build it (cargo build --release -p textdb-cli) or set TEXTDB_CLI.';
  }

  async run(prefix: string, options: RunOptions): Promise<unknown> {
    const link = this.link(prefix);
    const cli = this.cli;
    if (!cli) throw badRequest(this.unavailable ?? 'syncing is unavailable');
    if (options.base !== undefined && (!REV.test(options.base) || options.base.startsWith('-'))) {
      throw badRequest(`base must name a commit: ${options.base}`);
    }
    return this.exclusive(prefix, async () => {
      const args = ['--store', this.corpus.db, '--json'];
      // One argument, so a name starting with a dash is a name.
      if (options.author) args.push(`--author=${options.author}`);
      args.push('sync');
      if (options.dryRun) args.push('--dry-run');
      if (options.commit) args.push('--commit');
      if (options.base) args.push('--base', options.base);
      args.push('--', link.prefix, link.dir);
      const { stdout, stderr, status } = await runCli(cli, args);
      let parsed: Record<string, unknown> | null = null;
      try {
        parsed = JSON.parse(stdout) as Record<string, unknown>;
      } catch {
        // Not JSON: reported below.
      }
      // A report comes back whatever the exit status: conflicts (3) and blocked names (6) included.
      if (parsed && 'to_disk' in parsed) return parsed;
      const code = typeof parsed?.code === 'string' && /^TX00[0-4]$/.test(parsed.code) ? (parsed.code as ErrorCode) : 'TX000';
      const message = typeof parsed?.message === 'string' ? parsed.message : (stderr || stdout).trim().slice(0, 2000);
      throw new CodedError(code, message || `textdb sync exited with status ${status}`);
    });
  }

  /** A file the last sync left conflict markers in, as it is on disk. */
  async conflict(prefix: string, rel: string): Promise<{ rel: string; text: string }> {
    return { rel, text: readFileSync(await this.conflictFile(prefix, rel), 'utf8') };
  }

  /** Keep one side of every conflict in `rel`, then sync, so the resolution reaches the store. */
  async resolve(prefix: string, rel: string, keep: 'textdb' | 'disk', author: string | undefined): Promise<unknown> {
    const file = await this.conflictFile(prefix, rel);
    if (this.running.has(prefix)) throw new CodedError('TX002', `${prefix} is being synced already; try again when it finishes`);
    writeFileSync(file, resolveMarkers(readFileSync(file, 'utf8'), keep));
    return this.run(prefix, { author });
  }

  /** The folder `prefix` as this server syncs it; NotFound for any other. */
  link(prefix: string): SyncLinkConfig {
    const link = this.links.find((l) => l.prefix === prefix);
    if (!link) throw new NotFound(`${prefix} is not a folder this server syncs (TEXTDB_SYNC)`);
    return link;
  }

  private async state(link: SyncLinkConfig): Promise<SyncState | null> {
    let dir = link.dir;
    try {
      dir = realpathSync.native(link.dir);
    } catch {
      // Not there (yet): the record, if any, is under the configured path.
    }
    return this.corpus.syncState(link.prefix, dir);
  }

  /** Only files the last sync recorded as conflicted can be read or rewritten. */
  private async conflictFile(prefix: string, rel: string): Promise<string> {
    const link = this.link(prefix);
    if (!(await this.state(link))?.conflicts.includes(rel)) {
      throw new NotFound(`${rel} has no conflict markers from a sync of ${prefix}`);
    }
    return path.join(link.dir, ...rel.split('/'));
  }
}
