import { createHash } from 'node:crypto';
import { Readable } from 'node:stream';
import { type Corpus, NotFound, SORT_KEYS, type SortKey } from '@textdb/node';
import { Hono } from 'hono';
import { ZipFile } from 'yazl';
import { cors } from 'hono/cors';
import { type AssetService, fileStream } from './assets.ts';
import { badRequest, errorResponse } from './errors.ts';
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
}

function syncService(sync: SyncService | null | undefined): SyncService {
  if (!sync) throw new NotFound('no folders are set up for sync: set TEXTDB_SYNC on the server');
  return sync;
}

function assetService(assets: AssetService | null | undefined): AssetService {
  if (!assets) throw new NotFound('no folders are set up for sync: set TEXTDB_SYNC on the server');
  return assets;
}

const MAX_BULK_PATHS = 10_000;
/** Files hashed per export comparison request. */
const MAX_HASH_PATHS = 1000;

const LOCAL_ORIGIN =/^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/;

export function createApp(corpus: Corpus, hub: ChangeHub, options: AppOptions): Hono {
  const app = new Hono();
  app.onError((error, c) => errorResponse(c, error));
  app.use(
    '/api/*',
    cors({
      origin: (origin) => (LOCAL_ORIGIN.test(origin) ? origin : null),
      allowMethods: ['GET', 'PUT', 'POST', 'OPTIONS'],
      allowHeaders: ['Content-Type', 'Last-Event-ID'],
    }),
  );

  app.get('/api/info', (c) => c.json(corpus.info()));
  app.get('/api/ls', (c) => c.json(corpus.ls(c.req.query('path') || '/')));
  app.get('/api/list', (c) => {
    const sort = c.req.query('sort') || 'name';
    if (!SORT_KEYS.includes(sort as SortKey)) throw badRequest(`sort must be one of ${SORT_KEYS.join(', ')}`);
    const kind = c.req.query('kind');
    if (kind && kind !== 'file' && kind !== 'folder') throw badRequest('kind must be file or folder');
    const flag = c.req.query('recursive');
    return c.json(
      corpus.list(c.req.query('path') || '/', {
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
  app.get('/api/file', (c) => c.json(corpus.read(queryString(c, 'path'), queryInt(c, 'version'))));
  app.get('/api/chunks', (c) => c.json(corpus.chunks(queryString(c, 'path'), queryInt(c, 'version'))));
  app.get('/api/history', (c) => c.json(corpus.history(queryString(c, 'path'))));
  app.get('/api/hunks', (c) =>
    c.json(corpus.hunks(queryString(c, 'path'), queryInt(c, 'from'), queryInt(c, 'to'))),
  );
  app.get('/api/diff', (c) => {
    const from = parseInteger(queryString(c, 'from'), 'from');
    const to = parseInteger(queryString(c, 'to'), 'to');
    return c.json({ diff: corpus.diff(queryString(c, 'path'), from, to) });
  });
  app.get('/api/search', (c) =>
    c.json(
      corpus.search(queryString(c, 'q'), {
        prefix: c.req.query('prefix') || '/',
        limit: queryInt(c, 'limit') ?? 50,
      }),
    ),
  );

  app.put('/api/file', async (c) => {
    const body = await jsonBody(c);
    const result = corpus.write(bodyString(body, 'path'), bodyString(body, 'content'), {
      baseVersion: bodyOptionalInt(body, 'base_version'),
      author: bodyOptionalString(body, 'author'),
      message: bodyOptionalString(body, 'message'),
    });
    return c.json(result);
  });
  app.post('/api/replace-lines', async (c) => {
    const body = await jsonBody(c);
    const result = corpus.replaceLines(
      bodyString(body, 'path'),
      bodyInt(body, 'from'),
      bodyInt(body, 'to'),
      bodyString(body, 'text'),
      { baseVersion: bodyOptionalInt(body, 'base_version'), author: bodyOptionalString(body, 'author') },
    );
    return c.json(result);
  });

  app.get('/api/stat', (c) => c.json(corpus.stat(queryString(c, 'path'))));
  app.get('/api/entry', (c) => c.json(corpus.entry(queryString(c, 'path'))));

  // Sync with directories on this machine, configured by the operator (TEXTDB_SYNC).
  const sync = options.sync;
  app.get('/api/sync/links', (c) =>
    c.json(sync ? sync.list() : { available: false, reason: 'No folders are set up for sync: set TEXTDB_SYNC on the server.', links: [] }),
  );
  app.post('/api/sync', async (c) => {
    const body = await jsonBody(c);
    const report = await syncService(sync).run(bodyString(body, 'prefix'), {
      dryRun: body.dry_run === true,
      commit: body.commit === true,
      base: bodyOptionalString(body, 'base'),
      author: bodyOptionalString(body, 'author'),
    });
    return c.json(report);
  });
  app.get('/api/sync/conflict', (c) => c.json(syncService(sync).conflict(queryString(c, 'prefix'), queryString(c, 'rel'))));
  app.post('/api/sync/resolve', async (c) => {
    const body = await jsonBody(c);
    const keep = body.keep;
    if (keep !== 'textdb' && keep !== 'disk') throw badRequest('keep must be textdb or disk');
    const report = await syncService(sync).resolve(bodyString(body, 'prefix'), bodyString(body, 'rel'), keep, bodyOptionalString(body, 'author'));
    return c.json(report);
  });

  // Assets of the synced folders: their state, pull and push, and their files to show or download.
  const assets = options.assets;
  app.get('/api/assets', async (c) => c.json(await assetService(assets).status(queryString(c, 'prefix'), c.req.query('path') || undefined)));
  app.post('/api/assets/pull', async (c) => {
    const body = await jsonBody(c);
    return c.json(await assetService(assets).pull(bodyString(body, 'prefix'), bodyPaths(body), bodyOptionalString(body, 'author')));
  });
  app.post('/api/assets/push', async (c) => {
    const body = await jsonBody(c);
    const service = assetService(assets);
    return c.json(await service.push(bodyString(body, 'prefix'), bodyPaths(body), bodyOptionalString(body, 'message'), bodyOptionalString(body, 'author')));
  });
  app.get('/api/assets/file', async (c) => {
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
    return c.body(fileStream(f.file), 200, headers);
  });

  // Export. A client compares what is on its disk with these, then fetches only what differs.
  app.get('/api/export/files', (c) => {
    const dir = c.req.query('path') || '/';
    const files = corpus.exportFiles(dir).map(({ rel, nbytes, updated_at }) => ({ rel, nbytes, updated_at }));
    return c.json({ path: dir, files });
  });
  app.post('/api/export/hashes', async (c) => {
    const body = await jsonBody(c);
    const paths = body.paths;
    if (!Array.isArray(paths) || paths.some((p) => typeof p !== 'string')) throw badRequest('paths must be an array of strings');
    if (paths.length > MAX_HASH_PATHS) throw badRequest(`at most ${MAX_HASH_PATHS} paths per request`);
    const hashes = (paths as string[]).map((p) => ({ path: p, sha256: createHash('sha256').update(corpus.readBytes(p)).digest('hex') }));
    return c.json({ hashes });
  });
  app.get('/api/export/file', (c) =>
    c.body(new Uint8Array(corpus.readBytes(queryString(c, 'path'))), 200, {
      'Content-Type': 'application/octet-stream',
      'Cache-Control': 'no-store',
    }),
  );
  app.get('/api/export/zip', (c) => {
    const dir = c.req.query('path') || '/';
    const files = corpus.exportFiles(dir);
    const zip = new ZipFile();
    for (const f of files) {
      // Each entry is read from the store only when the archive reaches it, so a large folder
      // streams out without being held in memory.
      const lazy = Readable.from(
        (function* () {
          yield Buffer.from(corpus.readBytes(f.path));
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
    return c.json(corpus.bulk(op, paths as string[], options));
  });
  app.get('/api/path-history', (c) => {
    const id = queryInt(c, 'id');
    return c.json(id === undefined ? corpus.pathHistory(queryString(c, 'path')) : corpus.pathHistoryOf(id));
  });
  app.get('/api/setting', (c) => {
    const key = queryString(c, 'key');
    return c.json({ key, value: corpus.setting(key) });
  });
  app.put('/api/setting', async (c) => {
    const body = await jsonBody(c);
    const key = bodyString(body, 'key');
    return c.json({ key, value: corpus.setSetting(key, bodyOptionalString(body, 'value') ?? null) });
  });
  app.post('/api/move', async (c) => {
    const body = await jsonBody(c);
    const from = bodyString(body, 'from');
    const to = bodyString(body, 'to');
    corpus.move(from, to, { author: bodyOptionalString(body, 'author') });
    return c.json({ from, to });
  });
  app.post('/api/delete', async (c) => {
    const body = await jsonBody(c);
    const target = bodyString(body, 'path');
    corpus.remove(target, { author: bodyOptionalString(body, 'author') });
    return c.json({ path: target });
  });

  app.get('/api/trash', (c) => c.json(corpus.trash(queryInt(c, 'parent'))));
  app.get('/api/trash/entry', (c) => c.json(corpus.trashEntry(parseInteger(queryString(c, 'id'), 'id'))));
  app.get('/api/trash/file', (c) => {
    const id = parseInteger(queryString(c, 'id'), 'id');
    const version = queryInt(c, 'version');
    const entry = corpus.trashEntry(id);
    const content = corpus.trashRead(id, version);
    return c.json({ entry, version: version ?? entry.version, content });
  });
  app.get('/api/trash/history', (c) => c.json(corpus.trashHistory(parseInteger(queryString(c, 'id'), 'id'))));
  app.post('/api/trash/purge', async (c) => {
    const body = await jsonBody(c);
    return c.json(corpus.purge(bodyInt(body, 'id'), { author: bodyOptionalString(body, 'author') }));
  });
  app.post('/api/trash/empty', async (c) => {
    const body = await jsonBody(c);
    return c.json(corpus.emptyTrash({ author: bodyOptionalString(body, 'author') }));
  });

  app.post('/api/import', async (c) => {
    const body = await jsonBody(c);
    const files = bodyImportFiles(body);
    return c.json(corpus.importBatch(files, { author: bodyOptionalString(body, 'author') }));
  });

  app.get('/api/events', (c) => eventStream(c, hub, options.pingMs));
  app.all('/api/*', (c) => errorResponse(c, new NotFound(`no such endpoint: ${c.req.method} ${c.req.path}`)));
  app.get('*', webApp(options.webDist));
  return app;
}
