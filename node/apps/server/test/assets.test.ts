import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { after, before, describe, test } from 'node:test';
import { type RunningServer, startServer } from '../src/server.ts';
import { findCli, runCli } from '../src/sync.ts';
import { tempDir } from './helpers.ts';

const cli = findCli(process.env.TEXTDB_CLI);

describe('assets of a synced folder', { skip: cli ? false : 'the textdb CLI is not built (or set TEXTDB_CLI)' }, () => {
  let tmp: ReturnType<typeof tempDir>;
  let server: RunningServer;
  let notes: string;
  let bucket: string;
  let db: string;
  const png = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 1, 2, 3]);
  const previousConfig = process.env.TEXTDB_CONFIG_DIR;

  async function call(method: string, route: string, body?: unknown) {
    const res = await fetch(`${server.url}${route}`, {
      method,
      headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: (await res.json()) as Record<string, any> };
  }

  const status = (query = '') => call('GET', `/api/assets?prefix=/notes${query}`);
  const fileUrl = (assetPath: string, download = false) =>
    `${server.url}/api/assets/file?prefix=/notes&path=${encodeURIComponent(assetPath)}${download ? '&download=1' : ''}`;

  before(async () => {
    tmp = tempDir();
    // Asset store bindings and caches of the CLI the server runs stay in this test's folder.
    process.env.TEXTDB_CONFIG_DIR = path.join(tmp.dir, 'config');
    notes = path.join(tmp.dir, 'notes');
    bucket = path.join(tmp.dir, 'bucket');
    db = path.join(tmp.dir, 'kb.db');
    mkdirSync(path.join(notes, 'img'), { recursive: true });
    mkdirSync(bucket);
    writeFileSync(path.join(notes, 'a.md'), '![[pic.png]]\n');
    writeFileSync(path.join(notes, 'img', 'pic.png'), png);
    server = await startServer({
      db,
      port: 0,
      webDist: path.join(tmp.dir, 'web'),
      pingMs: 100,
      watchIntervalMs: 20,
      sync: [{ prefix: '/notes', dir: notes }],
      cli: cli ?? undefined,
    });
  });

  after(async () => {
    await server.close();
    tmp.remove();
    if (previousConfig === undefined) delete process.env.TEXTDB_CONFIG_DIR;
    else process.env.TEXTDB_CONFIG_DIR = previousConfig;
  });

  test('shows, pushes, serves and pulls the assets of the folder, and nothing outside it', async () => {
    assert.equal((await call('POST', '/api/sync', { prefix: '/notes' })).status, 200);

    // Before any asset store: a new image, named on disk.
    let s = await status();
    assert.equal(s.status, 200, JSON.stringify(s.body));
    assert.deepEqual(s.body.counts, { new: 1 });
    assert.deepEqual(
      [s.body.assets[0].path, s.body.assets[0].state, s.body.assets[0].type, s.body.assets[0].file],
      ['/notes/img/pic.png', 'new', 'image/png', 'img/pic.png'],
    );

    // Only the server's own folders, only paths inside them.
    assert.equal((await call('GET', '/api/assets?prefix=/elsewhere')).status, 404);
    assert.equal((await status('&path=/other/x.png')).status, 400);
    assert.equal((await status(`&path=${encodeURIComponent('/notes/../x.png')}`)).status, 400);
    assert.equal((await call('POST', '/api/assets/push', { prefix: '/notes', paths: ['/x.png'] })).status, 400);

    // Pushed to a store the CLI declares; a message starting with a dash is a message.
    const declared = await runCli(cli!, ['--store', db, 'assets', 'stores', '--add', 'team', '--root', bucket]);
    assert.equal(declared.status, 0, declared.stderr);
    const pushed = await call('POST', '/api/assets/push', { prefix: '/notes', message: '-pictures', author: 'web' });
    assert.equal(pushed.status, 200, JSON.stringify(pushed.body));
    assert.equal(pushed.body.pushed.length, 1, JSON.stringify(pushed.body));
    // The asset store mirrors the store's paths.
    assert.deepEqual(readFileSync(path.join(bucket, 'notes', 'img', 'pic.png')), png);
    const history = await call('GET', `/api/history?path=${encodeURIComponent('/notes/img/pic.png.tdbasset')}`);
    assert.equal(history.body[0].message, '-pictures');
    s = await status();
    assert.equal(s.body.assets[0].state, 'ok');
    assert.equal(s.body.assets[0].version, 1);
    assert.match(s.body.assets[0].sha256, /^[0-9a-f]{64}$/);

    // Served for a preview, and as a download; never sniffed, never run.
    let res = await fetch(fileUrl('/notes/img/pic.png'));
    assert.equal(res.status, 200);
    assert.equal(res.headers.get('content-type'), 'image/png');
    assert.equal(res.headers.get('x-content-type-options'), 'nosniff');
    assert.match(res.headers.get('content-security-policy') ?? '', /sandbox/);
    assert.match(res.headers.get('content-disposition') ?? '', /^inline/);
    assert.deepEqual(Buffer.from(await res.arrayBuffer()), png);
    res = await fetch(fileUrl('/notes/img/pic.png', true));
    assert.equal(res.headers.get('content-type'), 'application/octet-stream');
    assert.match(res.headers.get('content-disposition') ?? '', /^attachment; filename="pic.png"/);
    await res.arrayBuffer();
    assert.equal((await fetch(fileUrl('/notes/a.md'))).status, 404);

    // Not here: pull it first; pulled, it is served again.
    rmSync(path.join(notes, 'img', 'pic.png'));
    res = await fetch(fileUrl('/notes/img/pic.png'));
    assert.equal(res.status, 404);
    assert.match(((await res.json()) as { message: string }).message, /pull it first/);
    const pulled = await call('POST', '/api/assets/pull', { prefix: '/notes', paths: ['/notes/img/pic.png'] });
    assert.equal(pulled.status, 200, JSON.stringify(pulled.body));
    assert.equal(pulled.body.pulled.length, 1);
    assert.ok(existsSync(path.join(notes, 'img', 'pic.png')));
    assert.deepEqual(Buffer.from(await (await fetch(fileUrl('/notes/img/pic.png'))).arrayBuffer()), png);
  });
});
