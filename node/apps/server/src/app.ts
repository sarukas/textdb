import { type Corpus, NotFound } from '@textdb/node';
import { Hono } from 'hono';
import { cors } from 'hono/cors';
import { errorResponse } from './errors.ts';
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

const LOCAL_ORIGIN = /^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/;

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
