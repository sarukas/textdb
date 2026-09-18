import { createHash } from 'node:crypto';
import { Readable } from 'node:stream';
import { type CorpusApi, LINK_STATUSES, type LinkStatus, NotFound, SORT_KEYS, type SortKey } from '@textdb/node';
import type { Context } from 'hono';
import { Hono } from 'hono';
import type { Corpora } from './corpora.ts';
import { ZipFile } from 'yazl';
import { cors } from 'hono/cors';
import type { AccessService } from './access.ts';
import { type AssetService, fileStream } from './assets.ts';
import { badRequest, CodedError, errorResponse, unauthorized } from './errors.ts';
import { eventStream } from './events.ts';
import type { ChangeHub } from './hub.ts';
import {
  bodyImportFiles,
  bodyInt,
  bodyOptionalInt,
  bodyOptionalString,
  bodyPaths,
  bodyString,
  jsonBody,
  parseInteger,
  queryInt,
  queryString,
} from './params.ts';
import { webApp } from './static.ts';
import type { SyncService } from './sync.ts';

export interface AppOptions {
  webDist: string;
  pingMs: number;
  /** Folders synced with directories on this machine; null when none are set up. */
  sync?: SyncService | null;
  /** The assets of those folders; null when none are set up. */
  assets?: AssetService | null;
  /** Accounts, tokens and shares, run through the CLI. */
  access?: AccessService | null;
  /** The interface the server listens on; on loopback, requests must name a loopback host. */
  host?: string;
  /**
   * Answer nothing without a bearer.
   *
   * Off by default, where no bearer means the owner -- which is what opening the store is anyway,
   * and what a loopback server has always meant. On for a deployment that must not answer an
   * unauthenticated request: then the owner arrives with a token too, of an `admin`-kind account.
   */
  requireToken?: boolean;
}

/** Hono's per-request store: which corpus this request is answered from. */
type Vars = { Variables: { kb: CorpusApi } };

/** This request's corpus, set by the bearer middleware. */
function kbOf(c: Context<Vars>): CorpusApi {
  return c.get('kb');
}

/** The cookie a browser session keeps its bearer in; see `POST /api/session`. */
const SESSION_COOKIE = 'textdb_session';

/**
 * The bearer of this request: an `Authorization` header, else the session cookie.
 *
 * The cookie is there because two things a browser does cannot carry a header at all -- the change
 * feed, which is an `EventSource`, and an asset's bytes, which are an `<img src>` or a download.
 * The alternative is a token in a query string, which then lives in every log and every referrer.
 * It is `HttpOnly`, so no script reads it back, and `SameSite=Strict`, so another site cannot make
 * the browser send it.
 */
function bearerOf(c: Context<Vars>): string | undefined {
  const header = c.req.header('authorization') ?? '';
  const match = /^Bearer[ 	]+(.+)$/i.exec(header.trim());
  if (match?.[1]?.trim()) return match[1].trim();
  return cookieOf(c.req.header('cookie'), SESSION_COOKIE);
}

function cookieOf(header: string | undefined, name: string): string | undefined {
  for (const part of (header ?? '').split(';')) {
    const eq = part.indexOf('=');
    if (eq < 0) continue;
    if (part.slice(0, eq).trim() !== name) continue;
    return decodeURIComponent(part.slice(eq + 1).trim()) || undefined;
  }
  return undefined;
}

const LOOPBACK_HOSTS = new Set(['localhost', '127.0.0.1', '[::1]']);

function syncService(sync: SyncService | null | undefined): SyncService {
  if (!sync) throw new NotFound('no folders are set up for sync: set TEXTDB_SYNC on the server');
  return sync;
}

function assetService(assets: AssetService | null | undefined): AssetService {
  if (!assets) throw new NotFound('no folders are set up for sync: set TEXTDB_SYNC on the server');
  return assets;
}

function accessService(access: AccessService | null | undefined): AccessService {
  if (!access) throw new NotFound('this server cannot run the delegation commands: no textdb CLI was found');
  return access;
}

/**
 * Refuse a token session outright.
 *
 * For sync and assets, and for those only. Both work on directories of *this server's machine*,
 * configured by whoever started it, and both run the CLI against the store: an account's request
 * would either escalate (the CLI as the owner) or mean something not yet defined -- whose directory
 * is `/notes` when `/notes` is an alias? Said plainly rather than half-answered.
 */
function ownerOnly(c: Context<Vars>, what: string): void {
  if (bearerOf(c) === undefined) return;
  throw new CodedError('TX005', `${what} is the owner's: this server syncs directories of its own machine`);
}

const MAX_BULK_PATHS = 10_000;
/** Files hashed per export comparison request. */
const MAX_HASH_PATHS = 1000;

const LOCAL_ORIGIN =/^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/;

export function createApp(corpora: Corpora, hub: ChangeHub, options: AppOptions): Hono<Vars> {
  const app = new Hono<Vars>();
  app.onError((error, c) => errorResponse(c, error));
  // Listening on loopback, a request naming any other host reached it through a name that was
  // pointed at this computer (DNS rebinding): another site's page, which CORS would take for the
  // same origin.
  if (!options.host || ['127.0.0.1', 'localhost', '::1'].includes(options.host)) {
    app.use('/api/*', async (c, next) => {
      const host = (c.req.header('host') ?? '').toLowerCase().replace(/:\d+$/, '');
      if (!LOOPBACK_HOSTS.has(host)) throw badRequest(`this server answers requests for localhost only, not ${JSON.stringify(host)}`);
      await next();
    });
  }
  // Who is asking. A bearer is answered in that account's view, by a corpus of its own; without
  // one the store is opened as its owner, unless this deployment refuses that.
  app.use('/api/*', async (c, next) => {
    // Logging in is the one thing that cannot need a working session already: a stale cookie, or
    // none at all on a server that requires one, is exactly when this is called.
    if (c.req.path === '/api/session') return next();
    const bearer = bearerOf(c);
    if (bearer === undefined) {
      if (options.requireToken) {
        throw unauthorized('this server answers only authenticated requests: send Authorization: Bearer <token>');
      }
      c.set('kb', await corpora.owner());
    } else {
      c.set('kb', await corpora.forToken(bearer));
    }
    await next();
  });
  app.use(
    '/api/*',
    cors({
      origin: (origin) => (LOCAL_ORIGIN.test(origin) ? origin : null),
      allowMethods: ['GET', 'PUT', 'POST', 'OPTIONS'],
      allowHeaders: ['Authorization', 'Content-Type', 'Last-Event-ID'],
    }),
  );

  app.get('/api/info', async (c) => {
    const kb = kbOf(c);
    return c.json({ ...(await kb.info()), backend: kb.backend, capabilities: kb.capabilities, account: kb.account });
  });
  // Who this session is and what it can reach (docs/shapes.md, "Who is asking").
  app.get('/api/whoami', async (c) => c.json(await kbOf(c).whoami()));
  /**
   * Start a browser session with a bearer, or end one with `null`.
   *
   * The token is checked before the cookie is set -- by opening a corpus with it, which is what
   * the store refuses an unusable bearer on -- so a session is never handed out for a token that
   * does not work. The answer is `whoami`, so one round trip both logs in and says who that is.
   */
  app.post('/api/session', async (c) => {
    const body = await jsonBody(c);
    const token = bodyOptionalString(body, 'token');
    const secure = new URL(c.req.url).protocol === 'https:';
    const attrs = `Path=/; HttpOnly; SameSite=Strict${secure ? '; Secure' : ''}`;
    if (!token) {
      c.header('set-cookie', `${SESSION_COOKIE}=; Max-Age=0; ${attrs}`);
      const kb = await corpora.owner();
      return c.json(options.requireToken ? { account: null, admin: false, kind: 'none', namespace: 'none', shares: [] } : await kb.whoami());
    }
    const kb = await corpora.forToken(token);
    const who = await kb.whoami();
    c.header('set-cookie', `${SESSION_COOKIE}=${encodeURIComponent(token)}; ${attrs}`);
    return c.json(who);
  });
  // Both listing endpoints return the same `Entry[]`; `/api/list` wraps it in a page. `/api/ls`
  // used to be a bare unbounded array and `/api/list` a page without its own `limit` echoed.
  app.get('/api/ls', async (c) => c.json(await kbOf(c).ls(c.req.query('path') || '/')));
  app.get('/api/list', async (c) => {
    const sort = c.req.query('sort') || 'name';
    if (!SORT_KEYS.includes(sort as SortKey)) throw badRequest(`sort must be one of ${SORT_KEYS.join(', ')}`);
    const kind = c.req.query('kind');
    if (kind && kind !== 'file' && kind !== 'folder') throw badRequest('kind must be file or folder');
    const flag = c.req.query('recursive');
    return c.json(
      await kbOf(c).list(c.req.query('path') || '/', {
        sort: sort as SortKey,
        order: c.req.query('order') === 'desc' ? 'desc' : 'asc',
        offset: queryInt(c, 'offset'),
        limit: queryInt(c, 'limit'),
        recursive: flag === '1' || flag === 'true',
        name: c.req.query('name') || undefined,
        author: c.req.query('author'),
        type: c.req.query('type') || undefined,
        kind: (kind || undefined) as 'file' | 'folder' | undefined,
      }),
    );
  });
  app.get('/api/file', async (c) => c.json(await kbOf(c).read(queryString(c, 'path'), queryInt(c, 'version'))));
  app.get('/api/chunks', async (c) => c.json(await kbOf(c).chunks(queryString(c, 'path'), queryInt(c, 'version'))));
  app.get('/api/history', async (c) => c.json(await kbOf(c).history(queryString(c, 'path'))));
  app.get('/api/hunks', async (c) =>
    c.json(await kbOf(c).hunks(queryString(c, 'path'), queryInt(c, 'from'), queryInt(c, 'to'))),
  );
  app.get('/api/diff', async (c) => {
    const from = parseInteger(queryString(c, 'from'), 'from');
    const to = parseInteger(queryString(c, 'to'), 'to');
    return c.json({ diff: await kbOf(c).diff(queryString(c, 'path'), from, to) });
  });
  app.get('/api/search', async (c) =>
    c.json(
      await kbOf(c).search(queryString(c, 'q'), {
        prefix: c.req.query('prefix') || '/',
        // One default across the project rather than four: SQL said 100, this said 50, the UI
        // passed 200 and the CLI counted documents instead of rows.
        limit: queryInt(c, 'limit') ?? 200,
        perFile: queryInt(c, 'per_file') ?? 10,
      }),
    ),
  );

  // Links: the ones a document (or a folder) writes, and the ones pointing at it. Both return
  // the canonical ten-key row, so a client can treat the two directions alike.
  const linkOptions = (c: Context) => {
    const status = c.req.query('status');
    if (status && !LINK_STATUSES.includes(status as LinkStatus)) {
      throw badRequest(`status must be one of ${LINK_STATUSES.join(', ')}`);
    }
    return { status: (status as LinkStatus) || undefined, limit: queryInt(c, 'limit') ?? 10000 };
  };
  app.get('/api/links', async (c) => c.json(await kbOf(c).links(c.req.query('path') || '/', linkOptions(c))));
  app.get('/api/backlinks', async (c) => c.json(await kbOf(c).backlinks(c.req.query('path') || '/', linkOptions(c))));

  // Markdown headings: one document's outline, a folder's, or the whole store's. `names` is
  // the autosuggest call and answers from an index range.
  app.get('/api/outline', async (c) =>
    c.json(
      await kbOf(c).outline(c.req.query('path') || '/', {
        heading: c.req.query('heading') || undefined,
        match: (c.req.query('match') as 'exact' | 'prefix' | 'contains') || 'exact',
        level: queryInt(c, 'level') ?? undefined,
        limit: queryInt(c, 'limit') ?? 1000,
      }),
    ),
  );
  app.get('/api/outline/names', async (c) =>
    c.json(
      await kbOf(c).headingNames(c.req.query('path') || '/', {
        starts: c.req.query('starts') || '',
        limit: queryInt(c, 'limit') ?? 100,
      }),
    ),
  );

  // Front-matter property discovery. `keys` and `values` are the autosuggest calls and run on
  // every keystroke, so both take a prefix and answer from an index range.
  app.get('/api/meta/keys', async (c) =>
    c.json(
      await kbOf(c).propertyKeys({
        prefix: c.req.query('prefix') || '',
        limit: queryInt(c, 'limit') ?? 200,
      }),
    ),
  );
  app.get('/api/meta/values', async (c) =>
    c.json(
      await kbOf(c).propertyValues(queryString(c, 'key'), {
        prefix: c.req.query('prefix') || '',
        limit: queryInt(c, 'limit') ?? 200,
      }),
    ),
  );
  app.get('/api/meta/find', async (c) =>
    c.json(
      await kbOf(c).propertyFind(c.req.query('q') ?? '', {
        folder: c.req.query('folder') || '/',
        limit: queryInt(c, 'limit') ?? 500,
      }),
    ),
  );

  app.put('/api/file', async (c) => {
    const body = await jsonBody(c);
    const result = await kbOf(c).write(bodyString(body, 'path'), bodyString(body, 'content'), {
      baseVersion: bodyOptionalInt(body, 'base_version'),
      author: bodyOptionalString(body, 'author'),
      message: bodyOptionalString(body, 'message'),
    });
    return c.json(result);
  });
  app.post('/api/replace-lines', async (c) => {
    const body = await jsonBody(c);
    const result = await kbOf(c).replaceLines(
      bodyString(body, 'path'),
      bodyInt(body, 'from'),
      bodyInt(body, 'to'),
      bodyString(body, 'text'),
      { baseVersion: bodyOptionalInt(body, 'base_version'), author: bodyOptionalString(body, 'author') },
    );
    return c.json(result);
  });

  // `stat` is an alias: it returned a different, smaller record than `entry` for one path.
  app.get('/api/stat', async (c) => c.json(await kbOf(c).entry(queryString(c, 'path'))));
  app.get('/api/entry', async (c) => c.json(await kbOf(c).entry(queryString(c, 'path'))));

  // Sync with directories on this machine, configured by the operator (TEXTDB_SYNC).
  const sync = options.sync;
  app.get('/api/sync/links', async (c) => {
    ownerOnly(c, 'syncing');
    return c.json(sync ? await sync.list() : { available: false, reason: 'No folders are set up for sync: set TEXTDB_SYNC on the server.', links: [] });
  });
  app.post('/api/sync', async (c) => {
    ownerOnly(c, 'syncing');
    const body = await jsonBody(c);
    const report = await syncService(sync).run(bodyString(body, 'prefix'), {
      dryRun: body.dry_run === true,
      commit: body.commit === true,
      base: bodyOptionalString(body, 'base'),
      author: bodyOptionalString(body, 'author'),
    });
    return c.json(report);
  });
  app.get('/api/sync/conflict', async (c) => c.json(await syncService(sync).conflict(queryString(c, 'prefix'), queryString(c, 'rel'))));
  app.post('/api/sync/resolve', async (c) => {
    const body = await jsonBody(c);
    const keep = body.keep;
    if (keep !== 'textdb' && keep !== 'disk') throw badRequest('keep must be textdb or disk');
    const report = await syncService(sync).resolve(bodyString(body, 'prefix'), bodyString(body, 'rel'), keep, bodyOptionalString(body, 'author'));
    return c.json(report);
  });

  // Assets of the synced folders: their state, pull and push, and their files to show or download.
  const assets = options.assets;
  app.get('/api/assets', async (c) => {
    ownerOnly(c, 'the assets of a synced directory');
    return c.json(await assetService(assets).status(queryString(c, 'prefix'), c.req.query('path') || undefined));
  });
  app.post('/api/assets/pull', async (c) => {
    ownerOnly(c, 'pulling assets');
    const body = await jsonBody(c);
    return c.json(await assetService(assets).pull(bodyString(body, 'prefix'), bodyPaths(body), bodyOptionalString(body, 'author')));
  });
  app.post('/api/assets/push', async (c) => {
    ownerOnly(c, 'pushing assets');
    const body = await jsonBody(c);
    const service = assetService(assets);
    return c.json(await service.push(bodyString(body, 'prefix'), bodyPaths(body), bodyOptionalString(body, 'message'), bodyOptionalString(body, 'author')));
  });
  // The asset stores this textdb store declares: the owner's to change, and it is the store that
  // says so. A binding is this server's own machine and needs nothing of the store.
  app.get('/api/assets/stores', async (c) => c.json(await assetService(assets).stores(bearerOf(c))));
  app.post('/api/assets/stores', async (c) => {
    const body = await jsonBody(c);
    const service = assetService(assets);
    await service.putStore(bodyString(body, 'name'), bodyOptionalString(body, 'driver') ?? 'local', bodyString(body, 'root'), bearerOf(c));
    return c.json({ stores: await service.stores(bearerOf(c)) });
  });
  app.post('/api/assets/stores/remove', async (c) => {
    const body = await jsonBody(c);
    const service = assetService(assets);
    await service.removeStore(bodyString(body, 'name'), bearerOf(c));
    return c.json({ stores: await service.stores(bearerOf(c)) });
  });
  app.post('/api/assets/stores/bind', async (c) => {
    ownerOnly(c, 'binding an asset store to this machine');
    const body = await jsonBody(c);
    const service = assetService(assets);
    await service.bindStore(bodyString(body, 'name'), bodyOptionalString(body, 'location') ?? '');
    return c.json({ stores: await service.stores() });
  });
  app.post('/api/assets/relocate', async (c) => {
    ownerOnly(c, 'moving the files of assets in their store');
    const body = await jsonBody(c);
    return c.json(await assetService(assets).relocate(bodyString(body, 'prefix'), bodyPaths(body), bodyOptionalString(body, 'author')));
  });
  app.get('/api/assets/verify', async (c) => {
    ownerOnly(c, 'verifying the assets of a synced directory');
    return c.json(await assetService(assets).verify(queryString(c, 'prefix'), c.req.query('path') || undefined));
  });
  app.get('/api/assets/file', async (c) => {
    ownerOnly(c, 'the file of an asset on this server');
    // Hono answers HEAD through this handler and drops the body: no file is opened for one.
    const head = c.req.method === 'HEAD';
    const f = await assetService(assets).file(queryString(c, 'prefix'), queryString(c, 'path'));
    const download = c.req.query('download') === '1' || !f.inline;
    const headers: Record<string, string> = {
      'Content-Type': download ? 'application/octet-stream' : f.type,
      'Content-Length': String(f.size),
      'Content-Disposition': `${download ? 'attachment' : 'inline'}; filename="${f.name.replace(/[^\x20-\x7e]|["\\]/g, '_')}"; filename*=UTF-8''${encodeURIComponent(f.name)}`,
      'X-Content-Type-Options': 'nosniff',
      'Cache-Control': 'no-store',
    };
    // Images, audio and video shown on their own run nothing; the browser's PDF viewer needs its scripts.
    if (f.type !== 'application/pdf') headers['Content-Security-Policy'] = "sandbox; default-src 'none'";
    if (head) return c.body(null, 200, headers);
    return c.body(fileStream(f.file, f.size), 200, headers);
  });

  // ------------------------------------------------------------ accounts, tokens and shares
  //
  // Every one of these is the owner's, and it is the *store* that refuses a token session, so each
  // runs the CLI with this request's own bearer rather than as the owner. A rule re-decided here
  // would be a second opinion; the one that counts is the store's (see `AccessService`).
  const access = options.access;
  app.get('/api/access/accounts', async (c) => c.json(await accessService(access).accounts(bearerOf(c))));
  app.post('/api/access/accounts', async (c) => {
    const body = await jsonBody(c);
    await accessService(access).createAccount(
      bodyString(body, 'name'),
      bodyOptionalString(body, 'kind'),
      bodyOptionalString(body, 'root'),
      bearerOf(c),
    );
    return c.json({ accounts: await accessService(access).accounts(bearerOf(c)) });
  });
  app.post('/api/access/accounts/enabled', async (c) => {
    const body = await jsonBody(c);
    const enabled = body.enabled;
    if (typeof enabled !== 'boolean') throw badRequest('enabled must be true or false');
    await accessService(access).setAccountEnabled(bodyString(body, 'name'), enabled, bearerOf(c));
    return c.json({ accounts: await accessService(access).accounts(bearerOf(c)) });
  });
  // Turning a single-root account into one that holds shares under aliases: every path it sees
  // gains a `/<alias>` prefix, which is why it is a command and not a setting.
  app.post('/api/access/accounts/convert', async (c) => {
    const body = await jsonBody(c);
    await accessService(access).convertAccount(bodyString(body, 'name'), bodyOptionalString(body, 'alias'), bearerOf(c));
    return c.json({ accounts: await accessService(access).accounts(bearerOf(c)) });
  });

  app.get('/api/access/tokens', async (c) => c.json(await accessService(access).tokens(c.req.query('account') || undefined, bearerOf(c))));
  /** The bearer is in this answer and nowhere else: it is stored hashed and cannot be shown again. */
  app.post('/api/access/tokens', async (c) => {
    const body = await jsonBody(c);
    const minted = await accessService(access).createToken(
      bodyString(body, 'account'),
      bodyOptionalString(body, 'label'),
      bodyOptionalString(body, 'expires'),
      bearerOf(c),
    );
    return c.json(minted);
  });
  app.post('/api/access/tokens/revoke', async (c) => {
    const body = await jsonBody(c);
    await accessService(access).revokeToken(bodyInt(body, 'id'), bearerOf(c));
    return c.json({ tokens: await accessService(access).tokens(undefined, bearerOf(c)) });
  });

  // With `account`, that account's shares; with `path`, who can reach that path -- which is the
  // question a permissions view of a folder is asking.
  app.get('/api/access/shares', async (c) =>
    c.json(
      await accessService(access).shares(
        { account: c.req.query('account') || undefined, path: c.req.query('path') || undefined },
        bearerOf(c),
      ),
    ),
  );
  app.post('/api/access/shares', async (c) => {
    const body = await jsonBody(c);
    const share = await accessService(access).grant(
      bodyString(body, 'account'),
      bodyString(body, 'path'),
      bodyString(body, 'rights'),
      bodyOptionalString(body, 'alias'),
      bearerOf(c),
    );
    return c.json(share);
  });
  app.post('/api/access/shares/rename', async (c) => {
    const body = await jsonBody(c);
    await accessService(access).renameShare(bodyString(body, 'account'), bodyString(body, 'from'), bodyString(body, 'to'), bearerOf(c));
    return c.json({ shares: await accessService(access).shares({ account: bodyString(body, 'account') }, bearerOf(c)) });
  });
  app.post('/api/access/shares/revoke', async (c) => {
    const body = await jsonBody(c);
    await accessService(access).revokeShare(bodyString(body, 'account'), bodyString(body, 'alias'), bearerOf(c));
    return c.json({ shares: await accessService(access).shares({ account: bodyString(body, 'account') }, bearerOf(c)) });
  });

  // Export. A client compares what is on its disk with these, then fetches only what differs.
  app.get('/api/export/files', async (c) => {
    const dir = c.req.query('path') || '/';
    const files = (await kbOf(c).exportFiles(dir)).map(({ rel, nbytes, updated_at }) => ({ rel, nbytes, updated_at }));
    return c.json({ path: dir, files });
  });
  app.post('/api/export/hashes', async (c) => {
    const body = await jsonBody(c);
    const paths = body.paths;
    if (!Array.isArray(paths) || paths.some((p) => typeof p !== 'string')) throw badRequest('paths must be an array of strings');
    if (paths.length > MAX_HASH_PATHS) throw badRequest(`at most ${MAX_HASH_PATHS} paths per request`);
    const kb = kbOf(c);
    const hashes = await Promise.all(
      (paths as string[]).map(async (p) => ({
        path: p,
        sha256: createHash('sha256').update(await kb.readBytes(p)).digest('hex'),
      })),
    );
    return c.json({ hashes });
  });
  app.get('/api/export/file', async (c) =>
    c.body(new Uint8Array(await kbOf(c).readBytes(queryString(c, 'path'))), 200, {
      'Content-Type': 'application/octet-stream',
      'Cache-Control': 'no-store',
    }),
  );
  app.get('/api/export/zip', async (c) => {
    const dir = c.req.query('path') || '/';
    const kb = kbOf(c);
    const files = await kb.exportFiles(dir);
    const zip = new ZipFile();
    for (const f of files) {
      // Each entry is read from the store only when the archive reaches it, so a large folder
      // streams out without being held in memory.
      const lazy = Readable.from(
        (async function* () {
          yield Buffer.from(await kb.readBytes(f.path));
        })(),
      );
      zip.addReadStream(lazy, f.rel, { mtime: new Date(f.updated_at) });
    }
    zip.end();
    const name = `${dir === '/' ? 'corpus' : dir.slice(dir.lastIndexOf('/') + 1)}.zip`;
    return c.body(Readable.toWeb(zip.outputStream as Readable) as unknown as ReadableStream, 200, {
      'Content-Type': 'application/zip',
      'Content-Disposition': `attachment; filename="${name.replace(/[^\x20-\x7e]|["\\]/g, '_')}"; filename*=UTF-8''${encodeURIComponent(name)}`,
      'Cache-Control': 'no-store',
    });
  });
  app.post('/api/bulk', async (c) => {
    const body = await jsonBody(c);
    const op = bodyString(body, 'op');
    if (op !== 'move' && op !== 'delete') throw badRequest('op must be move or delete');
    const paths = body.paths;
    if (!Array.isArray(paths) || paths.length === 0 || paths.some((p) => typeof p !== 'string')) {
      throw badRequest('paths must be a non-empty array of strings');
    }
    if (paths.length > MAX_BULK_PATHS) throw badRequest(`at most ${MAX_BULK_PATHS} paths per request`);
    const options = { to: bodyOptionalString(body, 'to'), author: bodyOptionalString(body, 'author') };
    return c.json(await kbOf(c).bulk(op, paths as string[], options));
  });
  app.get('/api/path-history', async (c) => {
    const id = queryInt(c, 'id');
    return c.json(id === undefined ? await kbOf(c).pathHistory(queryString(c, 'path')) : await kbOf(c).pathHistoryOf(id));
  });
  app.get('/api/setting', async (c) => {
    const key = queryString(c, 'key');
    return c.json({ key, value: await kbOf(c).setting(key) });
  });
  app.put('/api/setting', async (c) => {
    const body = await jsonBody(c);
    const key = bodyString(body, 'key');
    return c.json({ key, value: await kbOf(c).setSetting(key, bodyOptionalString(body, 'value') ?? null) });
  });
  app.post('/api/move', async (c) => {
    const body = await jsonBody(c);
    const from = bodyString(body, 'from');
    const to = bodyString(body, 'to');
    await kbOf(c).move(from, to, { author: bodyOptionalString(body, 'author') });
    return c.json({ from, to });
  });
  app.post('/api/delete', async (c) => {
    const body = await jsonBody(c);
    const target = bodyString(body, 'path');
    await kbOf(c).remove(target, { author: bodyOptionalString(body, 'author') });
    return c.json({ path: target });
  });

  app.get('/api/trash', async (c) => c.json(await kbOf(c).trash(queryInt(c, 'parent'))));
  app.get('/api/trash/entry', async (c) => c.json(await kbOf(c).trashEntry(parseInteger(queryString(c, 'id'), 'id'))));
  app.get('/api/trash/file', async (c) => {
    const id = parseInteger(queryString(c, 'id'), 'id');
    const version = queryInt(c, 'version');
    const entry = await kbOf(c).trashEntry(id);
    const content = await kbOf(c).trashRead(id, version);
    return c.json({ entry, version: version ?? entry.version, content });
  });
  app.get('/api/trash/history', async (c) => c.json(await kbOf(c).trashHistory(parseInteger(queryString(c, 'id'), 'id'))));
  app.post('/api/trash/purge', async (c) => {
    const body = await jsonBody(c);
    return c.json(await kbOf(c).purge(bodyInt(body, 'id'), { author: bodyOptionalString(body, 'author') }));
  });
  app.post('/api/trash/empty', async (c) => {
    const body = await jsonBody(c);
    return c.json(await kbOf(c).emptyTrash({ author: bodyOptionalString(body, 'author') }));
  });

  app.post('/api/import', async (c) => {
    const body = await jsonBody(c);
    const files = bodyImportFiles(body);
    return c.json(await kbOf(c).importBatch(files, { author: bodyOptionalString(body, 'author') }));
  });

  app.get('/api/events', async (c) => eventStream(c, hub, options.pingMs));
  app.all('/api/*', async (c) => errorResponse(c, new NotFound(`no such endpoint: ${c.req.method} ${c.req.path}`)));
  app.get('*', webApp(options.webDist));
  return app;
}
