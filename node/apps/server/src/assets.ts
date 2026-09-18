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
  /**
   * ok, new, modified, outdated, conflict, not-pulled, conflict-copy, orphan, invalid-pointer or
   * invalid-path; and from what the asset store itself holds, changed-in-store, moved-here,
   * moved-in-store, trashed-in-store, ambiguous, invalid-item or not-permitted.
   */
  state: string;
  type: string;
  size?: number;
  store?: string;
  /** Where the asset store keeps its file, when that is not where the asset's own path says. */
  in_store?: string;
  sha256?: string;
  /** The pointer's version in the store. */
  version?: number;
  /** Its file on disk, relative to the folder's directory. */
  file?: string;
  note?: string;
}


/** One asset store, as `textdb assets stores --json` reports it. */
export interface AssetStoreRow {
  name: string;
  driver: string;
  /** The store-side identity: a folder, or an rclone remote path. Shared by everyone. */
  root: string;
  /** Where *this machine* reaches it, when it was bound locally. */
  bound_to: string | null;
  /** Which file or environment variable said so. */
  bound_by: string | null;
  reachable: boolean;
  problem: string | null;
}

/** What `assets verify --json` answers: each asset's two sides, and the store's loose files. */
export interface AssetVerification {
  prefix: string;
  dir: string;
  assets: { path: string; here: string; asset_store: string; note: string | null }[];
  unnamed: { store?: string; at?: string; unchecked?: string }[];
  problems: number;
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
 *
 * The states an asset store's own answer gives are safe for the same reason `ok` is: each of them
 * takes the place of `ok` alone, so the file here was already the asset's own bytes, and what they
 * say is about the store rather than about this file.
 */
const SENDABLE = new Set([
  'ok',
  'modified',
  'outdated',
  'new',
  'orphan',
  'conflict-copy',
  'changed-in-store',
  'moved-here',
  'moved-in-store',
  'trashed-in-store',
  'ambiguous',
  'invalid-item',
  // The store refused this computer the file: a fact about this computer's access, which only
  // ever replaces `ok` as the rest do, so the file on disk is still the asset's own bytes.
  'not-permitted',
]);

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

/** The stem of the 8.3 short name Windows gives `name`: its base without leading dots, spaces and dots, upper case, six characters. */
function shortStem(name: string): string {
  const trimmed = name.replace(/^\.+/, '');
  const dot = trimmed.lastIndexOf('.');
  const base = dot > 0 ? trimmed.slice(0, dot) : trimmed;
  return [...base.replace(/[ .]/g, '').toUpperCase()].slice(0, 6).join('');
}

const SHORT_STEMS = [...IGNORED_DIRS].map(shortStem);

/**
 * An 8.3 short name Windows may have given one of the ignored folders: GIT~1 for .git, or the
 * hashed form (GI3F2A~1). Keep in step with `short_name_of` in crates/textdb-cli/src/assets/classify.rs.
 */
function shortNameOfIgnored(seg: string): boolean {
  const m = /^([^~]{1,6})~\d+(\.[^.]{0,3})?$/.exec(seg);
  if (!m) return false;
  const stem = m[1]!.toUpperCase();
  return SHORT_STEMS.some((s) => stem === s || (stem.length === 6 && s.length >= 2 && stem.startsWith(s.slice(0, 2)) && /^[0-9A-F]{4}$/.test(stem.slice(2))));
}

/**
 * A store path at or below `prefix` that an asset can be at: no `.` or `..` part, backslash, colon
 * (an NTFS stream) or control character, and no part naming an ignored folder the way Windows
 * resolves names (any letter case, trailing dots and spaces dropped, or an 8.3 short name).
 */
function under(prefix: string, p: string): boolean {
  if (!p.startsWith('/') || p.includes('\\') || p.includes(':') || CONTROL.test(p)) return false;
  const segs = p.split('/');
  for (const s of segs) {
    if (s === '.' || s === '..') return false;
    const resolved = s.replace(/[. ]+$/, '');
    if (IGNORED_DIRS.has(resolved.toLowerCase()) || shortNameOfIgnored(resolved)) return false;
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
 *
 * The asset *stores* are the exception, and the reason `sync` may be absent: a store is declared on
 * the textdb store for everyone who opens it, and it is bound to a place on this machine. Neither
 * has anything to do with a folder this server syncs, and a server that syncs none could not
 * otherwise be told where the bytes live -- which is the configuration someone reaches for first.
 */
export class AssetService {
  private readonly sync: SyncService | null;
  private readonly db: string;
  /** The CLI to run when there is no sync service to take one from. */
  private readonly ownCli: string | null;
  private active = 0;
  private readonly waiting: (() => void)[] = [];

  constructor(sync: SyncService | null, db: string, cli: string | null = null) {
    this.sync = sync;
    this.db = db;
    this.ownCli = cli;
  }

  /** The folders this server syncs, or the refusal that there are none. */
  private get folders(): SyncService {
    if (!this.sync) throw new NotFound('no folders are set up for sync: set TEXTDB_SYNC on the server');
    return this.sync;
  }

  async status(prefix: string, scope?: string): Promise<AssetStatus> {
    const link = await this.syncedLink(prefix);
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

  async pull(prefix: string, paths: string[], author: string | undefined): Promise<Record<string, unknown>> {
    const link = await this.syncedLink(prefix);
    const targets = this.targets(link.prefix, paths);
    const who = argument('author', author);
    return this.folders.exclusive(link.prefix, async () => {
      await this.status(link.prefix);
      return this.run(['assets', 'pull', '--dir', link.dir, '--', ...targets], 'pulled', who);
    });
  }

  async push(prefix: string, paths: string[], message: string | undefined, author: string | undefined): Promise<Record<string, unknown>> {
    const link = await this.syncedLink(prefix);
    const targets = this.targets(link.prefix, paths);
    const args = ['assets', 'push', '--dir', link.dir];
    const text = argument('message', message);
    if (text) args.push(`--message=${text}`);
    args.push('--', ...targets);
    const who = argument('author', author);
    return this.folders.exclusive(link.prefix, async () => {
      await this.status(link.prefix);
      return this.run(args, 'pushed', who);
    });
  }

  /**
   * The asset stores this textdb store declares, with whether this machine can reach each one.
   *
   * Not scoped to a folder: a store is the store's, and the same one serves every vault. Every one
   * of these runs as the owner, because the routes are the owner's -- see the comment on them.
   *
   * Each of the three that change something answers with the whole list, because the CLI prints it
   * after the change: one run, and one set of reachability checks, rather than two.
   */
  stores(): Promise<AssetStoreRow[]> {
    return this.run(['assets', 'stores'], 'stores') as unknown as Promise<AssetStoreRow[]>;
  }

  putStore(name: string, driver: string, root: string): Promise<AssetStoreRow[]> {
    const args = [
      'assets',
      'stores',
      `--add=${argument('the store name', name)}`,
      `--driver=${argument('the driver', driver)}`,
      `--root=${argument('the root', root)}`,
    ];
    return this.run(args, 'stores') as unknown as Promise<AssetStoreRow[]>;
  }

  removeStore(name: string): Promise<AssetStoreRow[]> {
    return this.run(['assets', 'stores', `--remove=${argument('the store name', name)}`], 'stores') as unknown as Promise<AssetStoreRow[]>;
  }

  /**
   * Bind a store to where **this server's machine** reaches it, or clear the binding with ''.
   *
   * Per-machine configuration, written to this server's own config file, not to the textdb store:
   * the same shared drive is mounted differently by everyone, and that is the whole point of a
   * binding.
   *
   * `--bind` takes `NAME=LOCATION` and splits at the first `=`, so a name carrying one would bind
   * some other store: `team=evil` would leave `team` bound to `evil=...`, and the store it named
   * would not exist at all. The name is checked against the declared stores for the same reason --
   * a typo otherwise writes a binding into this machine's config that nothing ever shows.
   */
  async bindStore(name: string, location: string): Promise<AssetStoreRow[]> {
    const wanted = argument('the store name', name) ?? '';
    if (wanted.includes('=')) throw badRequest('an asset store name cannot contain "="');
    const declared = await this.stores();
    if (!declared.some((s) => s.name === wanted)) {
      throw new NotFound(`no asset store named ${wanted}: declare it before binding it to this machine`);
    }
    return this.run(['assets', 'stores', `--bind=${wanted}=${argument('the location', location)}`], 'stores') as unknown as Promise<
      AssetStoreRow[]
    >;
  }

  /** Move the files of `moved-here` assets to their asset's own place in the store. */
  async relocate(prefix: string, paths: string[], author: string | undefined): Promise<Record<string, unknown>> {
    const link = await this.syncedLink(prefix);
    const targets = this.targets(link.prefix, paths);
    const who = argument('author', author);
    return this.folders.exclusive(link.prefix, async () => {
      // As for a pull or a push: a directory the CLI now pairs with another store folder would
      // have its files moved in the asset store under a folder's name that is not theirs.
      await this.status(link.prefix);
      return this.run(['assets', 'relocate', '--dir', link.dir, '--', ...targets], 'moved', who);
    });
  }

  /**
   * Hash every asset here and in its store, and list the store's files no pointer names.
   *
   * `verify` exits 1 when it finds problems, which is an answer and not a failure: the report is
   * what was asked for.
   */
  async verify(prefix: string, scope?: string): Promise<AssetVerification> {
    const link = await this.syncedLink(prefix);
    const args = ['assets', 'verify', '--dir', link.dir];
    // A path narrows the check to one asset. Without one this is the whole vault, and only a
    // whole-vault verify lists the store's files that no pointer names -- so nothing is passed
    // rather than the folder itself, which would read as a scope and leave that list out.
    if (scope !== undefined) args.push('--', ...this.targets(link.prefix, [scope]));
    const report = (await this.run(args, 'assets')) as unknown as AssetVerification;
    // Unscoped, the CLI takes the folder from the directory's own sync record, so a directory now
    // paired with another store folder would be reported under a name that is not the one asked
    // about. The same check `status` makes, for the same reason.
    if (report.prefix !== link.prefix) {
      throw new CodedError(
        'TX001',
        `${link.dir} was synced last with ${report.prefix}, not ${link.prefix}: sync it with ${link.prefix} again before verifying its assets here`,
      );
    }
    return report;
  }

  /** The file on disk of the asset at `assetPath`: only an asset's own file, only inside the folder's directory. */
  async file(prefix: string, assetPath: string): Promise<AssetFile> {
    const link = this.folders.link(prefix);
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
  private async syncedLink(prefix: string): Promise<SyncLinkConfig> {
    const link = this.folders.link(prefix);
    const links = (await this.folders.list()).links;
    if (!links.find((l) => l.prefix === link.prefix)?.last) {
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
  private async run(args: string[], key: string, author?: string, bearer?: string): Promise<Record<string, unknown>> {
    const cli = this.sync?.cliPath ?? this.ownCli;
    if (!cli) throw badRequest(this.sync?.unavailable ?? 'the textdb CLI was not found');
    const global = ['--store', this.db, '--json'];
    // One argument, so a name starting with a dash is a name.
    if (author) global.push(`--author=${author}`);
    // The store decides who may do this, not this server: a command that is the owner's is refused
    // by the store when a bearer is presented, and allowed when an admin-kind one is.
    if (bearer) global.push(`--token=${bearer}`);
    const { stdout, stderr, status } = await this.slot(() => runCli(cli, [...global, ...args]));
    const printed = [...jsonLines(stdout), ...jsonLines(stderr)];
    const report = printed.find((o) => key in o);
    if (report) return report;
    // Some answers are a bare array -- `assets stores` is a list of stores and nothing else.
    if (status === 0) {
      const array = [stdout, stderr].map(asArray).find((a) => a !== undefined);
      if (array) return array as unknown as Record<string, unknown>;
    }
    // With --json the CLI prints its error as `{"error": {"code", "message", …}}`.
    const found = printed.find((o) => typeof o.error === 'object' && o.error !== null)?.error as Record<string, unknown> | undefined;
    // Every TX code, `TX005 forbidden` included: a refusal that arrived as a 500 would read as a
    // fault of the server's.
    // Every code this server knows how to answer with, `TX005 forbidden` included; a code from a
    // newer CLI than this server is a 500, because an answer it cannot map is not one it can pass on.
    const code = typeof found?.code === 'string' && /^TX00[0-5]$/.test(found.code) ? (found.code as ErrorCode) : 'TX000';
    const message = typeof found?.message === 'string' ? found.message : (stderr || stdout).trim().slice(0, 2000);
    throw new CodedError(code, message || `textdb ${args.slice(0, 2).join(' ')} exited with status ${status}`);
  }
}

/** A JSON array on its own, which is what a listing answers with. */
function asArray(text: string): unknown[] | undefined {
  const trimmed = text.trim();
  if (!trimmed.startsWith('[')) return undefined;
  try {
    const value: unknown = JSON.parse(trimmed);
    return Array.isArray(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

/** The first `size` bytes of `file`: as much as was looked at, whatever is written to it meanwhile. */
export function fileStream(file: string, size: number): ReadableStream {
  if (size === 0) return new ReadableStream({ start: (controller) => controller.close() });
  return Readable.toWeb(createReadStream(file, { start: 0, end: size - 1 })) as unknown as ReadableStream;
}
