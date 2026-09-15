import { createReadStream, realpathSync, statSync } from 'node:fs';
import path from 'node:path';
import { Readable } from 'node:stream';
import { type ErrorCode, NotFound } from '@textdb/node';
import type { SyncLinkConfig } from './config.ts';
import { badRequest, CodedError } from './errors.ts';
import { runCli, type SyncService } from './sync.ts';

/** One asset of a synced folder, as `textdb assets status --json` reports it. */
export interface AssetItem {
  path: string;
  /** ok, new, modified, outdated, conflict, not-pulled, conflict-copy, orphan, invalid-pointer or invalid-path. */
  state: string;
  type: string;
  size?: number;
  store?: string;
  sha256?: string;
  /** The pointer's version in the store. */
  version?: number;
  /** Its file on disk, relative to the folder's directory. */
  file?: string;
  note?: string;
}

export interface AssetStatus {
  prefix: string;
  dir: string;
  assets: AssetItem[];
  counts: Record<string, number>;
}

/** An asset's file, ready to send. */
export interface AssetFile {
  file: string;
  name: string;
  type: string;
  size: number;
  /** A type a browser may show as it is; anything else is only downloaded (SVG and HTML can run scripts). */
  inline: boolean;
}

/** Types sent inline; keep in step with `previewKind` in node/apps/web/src/assets/model.ts. */
const INLINE = new Set([
  'image/png',
  'image/jpeg',
  'image/gif',
  'image/webp',
  'image/avif',
  'image/bmp',
  'application/pdf',
  'audio/mpeg',
  'audio/ogg',
  'audio/wav',
  'audio/flac',
  'video/mp4',
  'video/webm',
]);

/**
 * The states whose file on disk may be sent: the asset's own bytes (ok), bytes this directory had
 * as that asset (modified, outdated), or a file the rules make an asset (new, orphan, conflict
 * copy). Never a file a pointer only names (conflict and the rest): anyone who can write a pointer
 * to the store must not be able to read any file of the directory through it.
 */
const SENDABLE = new Set(['ok', 'modified', 'outdated', 'new', 'orphan', 'conflict-copy']);

/** Folders textdb never reads assets from or writes them to; keep in step with IGNORED_DIRS in crates/textdb-cli/src/assets/classify.rs. */
const IGNORED_DIRS = new Set([
  '.git',
  '.textdb',
  '.trash',
  '.textdb-trash',
  'node_modules',
  '.obsidian',
  '__pycache__',
  '$recycle.bin',
  'system volume information',
  '.spotlight-v100',
  '.fseventsd',
  '.trashes',
]);

/** CLI runs at once: each walks the folder's directory and reads its pointers. */
const MAX_RUNS = 4;

const CONTROL = /[\x00-\x1f\x7f]/;

/** An 8.3 short name such as GIT~1: on Windows another name of whatever folder has it, .git included. */
const SHORT_NAME = /^[^~]{1,6}~\d+(\.[^.]{0,3})?$/;

/**
 * A store path at or below `prefix` that an asset can be at: no `.` or `..` part, backslash, colon
 * (an NTFS stream) or control character, and no part naming an ignored folder the way Windows
 * resolves names (any letter case, trailing dots and spaces dropped, or an 8.3 short name).
 */
function under(prefix: string, p: string): boolean {
  if (!p.startsWith('/') || p.includes('\\') || p.includes(':') || CONTROL.test(p)) return false;
  const segs = p.split('/');
  for (const [i, s] of segs.entries()) {
    if (s === '.' || s === '..') return false;
    if (IGNORED_DIRS.has(s.replace(/[. ]+$/, '').toLowerCase())) return false;
    if (i > 0 && i < segs.length - 1 && SHORT_NAME.test(s)) return false;
  }
  return prefix === '/' || p === prefix || p.startsWith(`${prefix}/`);
}

/** A name or message handed to the CLI: no NUL, which no command line can carry. */
function argument(name: string, value: string | undefined): string | undefined {
  if (value !== undefined && value.includes('\x00')) throw badRequest(`${name} must not contain a NUL character`);
  return value;
}

/** The JSON objects the CLI printed, one per line. */
function jsonLines(text: string): Record<string, unknown>[] {
  const out: Record<string, unknown>[] = [];
  for (const line of text.split(/\r?\n/)) {
    const t = line.trim();
    if (!t.startsWith('{')) continue;
    try {
      const v: unknown = JSON.parse(t);
      if (v && typeof v === 'object' && !Array.isArray(v)) out.push(v as Record<string, unknown>);
    } catch {
      // Not JSON.
    }
  }
  return out;
}

/**
 * The assets of the folders in `TEXTDB_SYNC`, through the textdb CLI as for sync: their state, pull
 * and push, and their files for previews and downloads. Only those folders and their directories,
 * only once a folder has been synced, and only while the CLI takes the directory for that folder
 * (not for another store folder it was also synced with); a pull or push takes the folder's turn
 * with its syncs.
 */
export class AssetService {
  private readonly sync: SyncService;
  private readonly db: string;
  private active = 0;
  private readonly waiting: (() => void)[] = [];

  constructor(sync: SyncService, db: string) {
    this.sync = sync;
    this.db = db;
  }

  async status(prefix: string, scope?: string): Promise<AssetStatus> {
    const link = this.syncedLink(prefix);
    const [target] = this.targets(link.prefix, scope === undefined ? [] : [scope]);
    const status = (await this.run(['assets', 'status', '--dir', link.dir, '--', target!], 'assets')) as unknown as AssetStatus;
    if (status.prefix !== link.prefix) {
      throw new CodedError(
        'TX001',
        `${link.dir} was synced last with ${status.prefix}, not ${link.prefix}: sync it with ${link.prefix} again before working with its assets here`,
      );
    }
    return status;
  }

  pull(prefix: string, paths: string[], author: string | undefined): Promise<Record<string, unknown>> {
    const link = this.syncedLink(prefix);
    const targets = this.targets(link.prefix, paths);
    const who = argument('author', author);
    return this.sync.exclusive(link.prefix, async () => {
      await this.status(link.prefix);
      return this.run(['assets', 'pull', '--dir', link.dir, '--', ...targets], 'pulled', who);
    });
  }

  push(prefix: string, paths: string[], message: string | undefined, author: string | undefined): Promise<Record<string, unknown>> {
    const link = this.syncedLink(prefix);
    const targets = this.targets(link.prefix, paths);
    const args = ['assets', 'push', '--dir', link.dir];
    const text = argument('message', message);
    if (text) args.push(`--message=${text}`);
    args.push('--', ...targets);
    const who = argument('author', author);
    return this.sync.exclusive(link.prefix, async () => {
      await this.status(link.prefix);
      return this.run(args, 'pushed', who);
    });
  }

  /** The file on disk of the asset at `assetPath`: only an asset's own file, only inside the folder's directory. */
  async file(prefix: string, assetPath: string): Promise<AssetFile> {
    const link = this.sync.link(prefix);
    const status = await this.status(link.prefix, assetPath);
    const item = status.assets.find((a) => a.path === assetPath);
    if (!item) throw new NotFound(`${assetPath} is not an asset of ${link.prefix}`);
    if (!item.file) throw new NotFound(`${assetPath} is not in ${link.dir}: pull it first`);
    if (!SENDABLE.has(item.state)) {
      throw new CodedError('TX001', `${assetPath} is ${item.state} here, so its file is not sent: ${item.note ?? 'see textdb assets status'}`);
    }
    let root: string;
    let full: string;
    try {
      root = realpathSync.native(link.dir);
      full = realpathSync.native(path.join(root, ...item.file.split('/')));
    } catch {
      throw new NotFound(`${item.file} is not in ${link.dir}`);
    }
    const rel = path.relative(root, full);
    if (!rel || rel === '..' || rel.startsWith(`..${path.sep}`) || path.isAbsolute(rel)) throw new NotFound(`${item.file} is not in ${link.dir}`);
    const stat = statSync(full);
    if (!stat.isFile()) throw new NotFound(`${item.file} is not a file`);
    return { file: full, name: path.basename(full), type: item.type, size: stat.size, inline: INLINE.has(item.type) };
  }

  /** The folder as this server syncs it, once it has been synced (before, the CLI would take a path inside it for the folder). */
  private syncedLink(prefix: string): SyncLinkConfig {
    const link = this.sync.link(prefix);
    if (!this.sync.list().links.find((l) => l.prefix === link.prefix)?.last) {
      throw new CodedError('TX004', `${link.prefix} has not been synced with ${link.dir} yet: sync it first`);
    }
    return link;
  }

  /** The folder itself when no paths are given; otherwise the paths, each at or below it. */
  private targets(prefix: string, paths: string[]): string[] {
    for (const p of paths) if (!under(prefix, p)) throw badRequest(`${JSON.stringify(p)} is not a path in ${prefix} that assets can be at`);
    return paths.length ? paths : [prefix];
  }

  /** At most MAX_RUNS CLI runs at once; the others wait their turn, handed over as one ends. */
  private async slot<T>(fn: () => Promise<T>): Promise<T> {
    if (this.active < MAX_RUNS) this.active++;
    else await new Promise<void>((resolve) => this.waiting.push(resolve));
    try {
      return await fn();
    } finally {
      const next = this.waiting.shift();
      if (next) next();
      else this.active--;
    }
  }

  /**
   * `textdb --json ARGS`: the printed report, the object with `key`, whatever the exit status (a
   * push that left conflicts exits 3 and still reports); otherwise the CLI's error.
   */
  private async run(args: string[], key: string, author?: string): Promise<Record<string, unknown>> {
    const cli = this.sync.cliPath;
    if (!cli) throw badRequest(this.sync.list().reason ?? 'the textdb CLI was not found');
    const global = ['--store', this.db, '--json'];
    // One argument, so a name starting with a dash is a name.
    if (author) global.push(`--author=${author}`);
    const { stdout, stderr, status } = await this.slot(() => runCli(cli, [...global, ...args]));
    const printed = [...jsonLines(stdout), ...jsonLines(stderr)];
    const report = printed.find((o) => key in o);
    if (report) return report;
    // With --json the CLI prints its error as `{"error": {"code", "message", …}}`.
    const found = printed.find((o) => typeof o.error === 'object' && o.error !== null)?.error as Record<string, unknown> | undefined;
    const code = typeof found?.code === 'string' && /^TX00[0-4]$/.test(found.code) ? (found.code as ErrorCode) : 'TX000';
    const message = typeof found?.message === 'string' ? found.message : (stderr || stdout).trim().slice(0, 2000);
    throw new CodedError(code, message || `textdb ${args.slice(0, 2).join(' ')} exited with status ${status}`);
  }
}

/** The first `size` bytes of `file`: as much as was looked at, whatever is written to it meanwhile. */
export function fileStream(file: string, size: number): ReadableStream {
  if (size === 0) return new ReadableStream({ start: (controller) => controller.close() });
  return Readable.toWeb(createReadStream(file, { start: 0, end: size - 1 })) as unknown as ReadableStream;
}
