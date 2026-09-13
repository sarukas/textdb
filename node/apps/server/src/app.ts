import { type Corpus, NotFound, SORT_KEYS, type SortKey } from '@textdb/node';
import { Hono } from 'hono';
import { cors } from 'hono/cors';
import { badRequest, errorResponse } from './errors.ts';
import { eventStream } from './events.ts';
import type { ChangeHub } from './hub.ts';
import {
  bodyImportFiles,
  bodyInt,
  bodyOptionalInt,
  bodyOptionalString,
  bodyString,
  jsonBody,
  parseInteger,
  queryInt,
  queryString,
} from './params.ts';
import { webApp } from './static.ts';

export interface AppOptions {
  webDist: string;
  pingMs: number;
}

const MAX_BULK_PATHS = 10_000;

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
