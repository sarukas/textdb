import { createReadStream, realpathSync, statSync } from 'node:fs';
import path from 'node:path';
import { Readable } from 'node:stream';
import { type ErrorCode, NotFound } from '@textdb/node';
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

/** A store path at or below `prefix`, with no `.` or `..` part. */
function under(prefix: string, p: string): boolean {
  if (!p.startsWith('/') || p.split('/').some((s) => s === '.' || s === '..')) return false;
  return prefix === '/' || p === prefix || p.startsWith(`${prefix}/`);
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
 * and push, and their files for previews and downloads. Only those folders and their directories;
 * a pull or push takes the folder's turn with its syncs.
 */
export class AssetService {
  private readonly sync: SyncService;
  private readonly db: string;

  constructor(sync: SyncService, db: string) {
    this.sync = sync;
    this.db = db;
  }

  async status(prefix: string, scope?: string): Promise<AssetStatus> {
    const link = this.sync.link(prefix);
    const [target] = this.targets(link.prefix, scope === undefined ? [] : [scope]);
    return (await this.run(['assets', 'status', '--dir', link.dir, '--', target!], 'assets')) as unknown as AssetStatus;
  }

  pull(prefix: string, paths: string[], author: string | undefined): Promise<Record<string, unknown>> {
    const link = this.sync.link(prefix);
    const targets = this.targets(link.prefix, paths);
    return this.sync.exclusive(link.prefix, () => this.run(['assets', 'pull', '--dir', link.dir, '--', ...targets], 'pulled', author));
  }

  push(prefix: string, paths: string[], message: string | undefined, author: string | undefined): Promise<Record<string, unknown>> {
    const link = this.sync.link(prefix);
    const targets = this.targets(link.prefix, paths);
    const args = ['assets', 'push', '--dir', link.dir];
    if (message) args.push(`--message=${message}`);
    args.push('--', ...targets);
    return this.sync.exclusive(link.prefix, () => this.run(args, 'pushed', author));
  }

  /** The file on disk of the asset at `assetPath`: only an asset of the folder, only inside its directory. */
  async file(prefix: string, assetPath: string): Promise<AssetFile> {
    const link = this.sync.link(prefix);
    const status = await this.status(link.prefix, assetPath);
    const item = status.assets.find((a) => a.path === assetPath);
    if (!item) throw new NotFound(`${assetPath} is not an asset of ${link.prefix}`);
    if (!item.file) throw new NotFound(`${assetPath} is not in ${link.dir}: pull it first`);
    let root: string;
    let full: string;
    try {
      root = realpathSync.native(link.dir);
      full = realpathSync.native(path.join(root, ...item.file.split('/')));
    } catch {
      throw new NotFound(`${item.file} is not in ${link.dir}`);
    }
    const rel = path.relative(root, full);
    if (!rel || rel.startsWith('..') || path.isAbsolute(rel)) throw new NotFound(`${item.file} is not in ${link.dir}`);
    const stat = statSync(full);
    if (!stat.isFile()) throw new NotFound(`${item.file} is not a file`);
    return { file: full, name: path.basename(full), type: item.type, size: stat.size, inline: INLINE.has(item.type) };
  }

  /** The folder itself when no paths are given; otherwise the paths, each at or below it. */
  private targets(prefix: string, paths: string[]): string[] {
    for (const p of paths) if (!under(prefix, p)) throw badRequest(`${p} is not in ${prefix}`);
    return paths.length ? paths : [prefix];
  }

  /**
   * `textdb --json ARGS`: the printed report, the object with `key`, whatever the exit status (a
   * push that left conflicts exits 3 and still reports); otherwise the CLI's error.
   */
  private async run(args: string[], key: string, author?: string): Promise<Record<string, unknown>> {
    const cli = this.sync.cliPath;
    if (!cli) throw badRequest(this.sync.list().reason ?? 'the textdb CLI was not found');
    const global = ['--store', this.db, '--json'];
    if (author) global.push('--author', author);
    const { stdout, stderr, status } = await runCli(cli, [...global, ...args]);
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

export function fileStream(file: string): ReadableStream {
  return Readable.toWeb(createReadStream(file)) as unknown as ReadableStream;
}
