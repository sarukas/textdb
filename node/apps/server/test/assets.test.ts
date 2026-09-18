import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
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
  const NUL = String.fromCharCode(0);

  async function call(method: string, route: string, body?: unknown) {
    const res = await fetch(`${server.url}${route}`, {
      method,
      headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: (await res.json()) as Record<string, any> };
  }

  /** A GET naming `host` in its Host header, as a browser does after DNS rebinding. */
  function getWithHost(route: string, host: string): Promise<number> {
    const url = new URL(`${server.url}${route}`);
    return new Promise((resolve, reject) => {
      http
        .get({ hostname: url.hostname, port: url.port, path: `${url.pathname}${url.search}`, headers: { Host: host } }, (res) => {
          res.resume();
          resolve(res.statusCode ?? 0);
        })
        .on('error', reject);
    });
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
      store: db,
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
    // Not before the folder has been synced: the CLI would take a path inside it for the folder.
    const early = await status('&path=/notes/img');
    assert.equal(early.status, 400, JSON.stringify(early.body));
    assert.match(early.body.message, /sync it first/);
    assert.equal((await call('POST', '/api/sync', { prefix: '/notes' })).status, 200);

    // Writes only as JSON: another site's plain-text POST is refused before anything runs.
    const plain = await fetch(`${server.url}/api/assets/pull`, {
      method: 'POST',
      headers: { 'Content-Type': 'text/plain' },
      body: JSON.stringify({ prefix: '/notes' }),
    });
    assert.equal(plain.status, 400);
    await plain.text();
    // Nor through another name pointed at this computer.
    assert.equal(await getWithHost('/api/assets?prefix=/notes', 'attacker.example'), 400);
    assert.equal(await getWithHost('/api/info', 'localhost:4317'), 200);

    // Before any asset store: a new image, named on disk.
    let s = await status();
    assert.equal(s.status, 200, JSON.stringify(s.body));
    assert.deepEqual(s.body.counts, { new: 1 });
    assert.deepEqual(
      [s.body.assets[0].path, s.body.assets[0].state, s.body.assets[0].type, s.body.assets[0].file],
      ['/notes/img/pic.png', 'new', 'image/png', 'img/pic.png'],
    );

    // Only the server's own folders, only paths inside them that assets can be at.
    assert.equal((await call('GET', '/api/assets?prefix=/elsewhere')).status, 404);
    assert.equal((await status('&path=/other/x.png')).status, 400);
    assert.equal((await status(`&path=${encodeURIComponent('/notes/../x.png')}`)).status, 400);
    assert.equal((await status(`&path=${encodeURIComponent(`/notes/a${NUL}.png`)}`)).status, 400);
    assert.equal((await status(`&path=${encodeURIComponent('/notes\\..\\x.png')}`)).status, 400);
    assert.equal((await status(`&path=${encodeURIComponent('/notes/.GIT/hooks/post-checkout')}`)).status, 400);
    // Nor through the other names Windows gives .git: an 8.3 short name, trailing dots, a stream.
    for (const alias of ['/notes/GIT~1/hooks/post-checkout', '/notes/.git./hooks/x', '/notes/.git::$INDEX_ALLOCATION/hooks/x', '/notes/GI3F2A~1/hooks/x', '/notes/sub/GIT~1']) {
      assert.equal((await status(`&path=${encodeURIComponent(alias)}`)).status, 400, alias);
    }
    // A folder or file that only looks like a short name is fine.
    assert.equal((await status(`&path=${encodeURIComponent('/notes/photos~1/p.png')}`)).status, 200);
    assert.equal((await call('POST', '/api/assets/push', { prefix: '/notes', paths: ['/x.png'] })).status, 400);
    assert.equal((await call('POST', '/api/assets/pull', { prefix: '/notes', author: `a${NUL}b` })).status, 400);

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
    res = await fetch(fileUrl('/notes/img/pic.png'), { method: 'HEAD' });
    assert.equal(res.status, 200);
    assert.equal(res.headers.get('content-length'), String(png.length));
    assert.equal((await res.arrayBuffer()).byteLength, 0);

    // A pointer anyone wrote to the store names a file that is not an asset: nothing of that file
    // is told or sent.
    writeFileSync(path.join(notes, '.env'), 'SECRET=hunter2\n');
    const pointerText = (await call('GET', `/api/file?path=${encodeURIComponent('/notes/img/pic.png.tdbasset')}`)).body.content;
    assert.equal((await call('PUT', '/api/file', { path: '/notes/.env.tdbasset', content: pointerText })).status, 200);
    const env = (await status('&path=/notes/.env')).body.assets.find((a: { path: string }) => a.path === '/notes/.env');
    assert.equal(env.state, 'conflict');
    assert.equal(env.size, undefined);
    assert.equal(env.file, undefined);
    res = await fetch(fileUrl('/notes/.env'));
    assert.equal(res.status, 404);
    assert.doesNotMatch(await res.text(), /hunter2/);

    // Not here: pull it first; pulled, it is served again.
    rmSync(path.join(notes, 'img', 'pic.png'));
    res = await fetch(fileUrl('/notes/img/pic.png'));
    assert.equal(res.status, 404);
    assert.match(((await res.json()) as { message: string }).message, /pull it first/);
    // An author starting with a dash is an author.
    const pulled = await call('POST', '/api/assets/pull', { prefix: '/notes', paths: ['/notes/img/pic.png'], author: '-web' });
    assert.equal(pulled.status, 200, JSON.stringify(pulled.body));
    assert.equal(pulled.body.pulled.length, 1);
    assert.ok(existsSync(path.join(notes, 'img', 'pic.png')));
    assert.deepEqual(Buffer.from(await (await fetch(fileUrl('/notes/img/pic.png'))).arrayBuffer()), png);
  });
});

/**
 * The asset stores themselves, over HTTP.
 *
 * A store is declared once for everyone -- name, driver, and the root they all know it by -- and
 * *bound* on each machine to where that machine reaches it. The web app configures both, so the
 * server has to keep them apart: the declaration goes to the textdb store, the binding to this
 * server's own config file. The refusals are the store's, and arrive as codes rather than as
 * this server's opinion.
 */
describe('asset stores over HTTP', { skip: cli ? false : 'the textdb CLI is not built (or set TEXTDB_CLI)' }, () => {
  let tmp: ReturnType<typeof tempDir>;
  let server: RunningServer;
  let notes: string;
  let bucket: string;
  const previousConfig = process.env.TEXTDB_CONFIG_DIR;
  const png = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 4, 5, 6]);

  async function call(method: string, route: string, body?: unknown) {
    const res = await fetch(`${server.url}${route}`, {
      method,
      headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: (await res.json()) as Record<string, any> };
  }

  before(async () => {
    tmp = tempDir();
    process.env.TEXTDB_CONFIG_DIR = path.join(tmp.dir, 'config');
    notes = path.join(tmp.dir, 'notes');
    bucket = path.join(tmp.dir, 'bucket');
    mkdirSync(path.join(notes, 'img'), { recursive: true });
    mkdirSync(bucket);
    writeFileSync(path.join(notes, 'img', 'pic.png'), png);
    server = await startServer({
      store: path.join(tmp.dir, 'kb.db'),
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

  test('declared for everyone, bound for this machine, and not removed while in use', async () => {
    assert.deepEqual((await call('GET', '/api/assets/stores')).body, []);

    // Declared: the root is what every machine knows it by.
    const added = await call('POST', '/api/assets/stores', { name: 'team', driver: 'local', root: bucket });
    assert.equal(added.status, 200, JSON.stringify(added.body));
    assert.deepEqual(
      added.body.stores.map((s: Record<string, unknown>) => [s.name, s.driver, s.root, s.bound_to, s.reachable]),
      [['team', 'local', bucket, null, true]],
    );

    // Bound: where *this* machine reaches it, which is this server's own configuration and not the
    // store's -- so nothing about the declaration moves.
    const elsewhere = path.join(tmp.dir, 'mounted');
    mkdirSync(elsewhere);
    const bound = await call('POST', '/api/assets/stores/bind', { name: 'team', location: elsewhere });
    assert.equal(bound.status, 200, JSON.stringify(bound.body));
    assert.deepEqual(
      bound.body.stores.map((s: Record<string, unknown>) => [s.root, s.bound_to, s.reachable]),
      [[bucket, elsewhere, true]],
    );
    assert.ok(typeof bound.body.stores[0].bound_by === 'string', 'the binding says which file it came from');

    // A binding this machine cannot follow is a fact about this machine: reported, not an error.
    const broken = await call('POST', '/api/assets/stores/bind', { name: 'team', location: path.join(tmp.dir, 'no-such-mount') });
    assert.equal(broken.status, 200, JSON.stringify(broken.body));
    assert.equal(broken.body.stores[0].reachable, false);
    assert.ok(broken.body.stores[0].problem, 'and says what is wrong with it');

    // Cleared, and the store is reachable at its root again.
    const cleared = await call('POST', '/api/assets/stores/bind', { name: 'team', location: '' });
    assert.deepEqual(
      cleared.body.stores.map((s: Record<string, unknown>) => [s.bound_to, s.reachable]),
      [[null, true]],
    );

    // With bytes in it, removing the store is refused: the files would have to move with it.
    assert.equal((await call('POST', '/api/sync', { prefix: '/notes' })).status, 200);
    const pushed = await call('POST', '/api/assets/push', { prefix: '/notes', author: 'web' });
    assert.equal(pushed.status, 200, JSON.stringify(pushed.body));
    assert.equal(pushed.body.pushed.length, 1);
    const refused = await call('POST', '/api/assets/stores/remove', { name: 'team' });
    assert.equal(refused.status, 400, JSON.stringify(refused.body));
    assert.equal(refused.body.code, 'TX004');
    assert.match(refused.body.message, /1 pointer names its files/);
    // And it is still there, unchanged.
    assert.deepEqual((await call('GET', '/api/assets/stores')).body.map((s: { name: string }) => s.name), ['team']);
  });

  test('what cannot be a command line argument is refused as a bad request', async () => {
    // A NUL cannot be in an argument at all, and Node throws an internal TypeError when one is:
    // that is a 400 about the request, not a 500 about this server.
    const nul = await call('POST', '/api/assets/stores', { name: 'a b', root: bucket });
    assert.equal(nul.status, 400, JSON.stringify(nul.body));
    assert.equal(nul.body.code, 'TX004');

    // `--bind` splits its argument at the first `=`, so a name carrying one would bind a different
    // store and leave it pointing at nothing.
    const equals = await call('POST', '/api/assets/stores/bind', { name: 'team=evil', location: 'z' });
    assert.equal(equals.status, 400, JSON.stringify(equals.body));
    assert.deepEqual((await call('GET', '/api/assets/stores')).body.map((s: Record<string, unknown>) => [s.name, s.bound_to]), [['team', null]]);

    // And a binding is only ever written for a store that is declared: a typo otherwise lands in
    // this machine's config file where nothing shows it again.
    const typo = await call('POST', '/api/assets/stores/bind', { name: 'teem', location: bucket });
    assert.equal(typo.status, 404, JSON.stringify(typo.body));
  });

  test('verify answers both sides of every asset, and the store’s files no pointer names', async () => {
    const verified = await call('GET', '/api/assets/verify?prefix=/notes');
    assert.equal(verified.status, 200, JSON.stringify(verified.body));
    assert.equal(verified.body.problems, 0);
    assert.deepEqual(
      verified.body.assets.map((a: Record<string, unknown>) => [a.path, a.here, a.asset_store]),
      [['/notes/img/pic.png', 'ok', 'ok']],
    );
    assert.deepEqual(verified.body.unnamed, []);

    // A file somebody put in the store by hand. Told of, never counted as a problem -- whose bytes
    // those are is not textdb's to decide -- and only for a whole vault, which is why a verify of
    // the folder passes no scope at all.
    writeFileSync(path.join(bucket, 'notes', 'stray.png'), png);
    const loose = await call('GET', '/api/assets/verify?prefix=/notes');
    assert.deepEqual(
      loose.body.unnamed.map((u: Record<string, unknown>) => [u.store, u.at]),
      [['team', '/notes/stray.png']],
    );
    assert.equal(loose.body.problems, 0);

    // One asset's own check is one asset's: the drive is not listed for it.
    const one = await call('GET', `/api/assets/verify?prefix=/notes&path=${encodeURIComponent('/notes/img/pic.png')}`);
    assert.equal(one.body.assets.length, 1);
    assert.deepEqual(one.body.unnamed, []);

    // Relocate answers a report of what it moved. Nothing here: a store that keeps its files by
    // path holds them where the pointer says, and it is a store with ids of its own -- a drive --
    // whose files stay put when an asset moves.
    const relocated = await call('POST', '/api/assets/relocate', { prefix: '/notes', author: 'web' });
    assert.equal(relocated.status, 200, JSON.stringify(relocated.body));
    assert.deepEqual([relocated.body.moved, relocated.body.failed], [[], []]);
  });
});

/**
 * A server that syncs no folder still configures where the bytes live.
 *
 * Asset stores belong to the textdb store, not to a folder this server happens to sync, and the
 * binding belongs to this machine. Someone setting a vault up declares them before there is
 * anything to pull, so refusing the whole panel for want of `TEXTDB_SYNC` would refuse the first
 * step. The folder-scoped calls still say there are no folders, because there are none.
 */
describe('asset stores without a synced folder', { skip: cli ? false : 'the textdb CLI is not built (or set TEXTDB_CLI)' }, () => {
  let tmp: ReturnType<typeof tempDir>;
  let server: RunningServer;
  const previousConfig = process.env.TEXTDB_CONFIG_DIR;

  before(async () => {
    tmp = tempDir();
    process.env.TEXTDB_CONFIG_DIR = path.join(tmp.dir, 'config');
    server = await startServer({
      store: path.join(tmp.dir, 'kb.db'),
      port: 0,
      webDist: path.join(tmp.dir, 'web'),
      cli: cli ?? undefined,
    });
  });

  after(async () => {
    await server.close();
    tmp.remove();
    if (previousConfig === undefined) delete process.env.TEXTDB_CONFIG_DIR;
    else process.env.TEXTDB_CONFIG_DIR = previousConfig;
  });

  test('the stores are listed and declared; the folder calls say there is no folder', async () => {
    const get = async (url: string) => {
      const res = await fetch(`${server.url}${url}`);
      return { status: res.status, body: (await res.json()) as Record<string, any> };
    };
    const post = async (url: string, body: unknown) => {
      const res = await fetch(`${server.url}${url}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      return { status: res.status, body: (await res.json()) as Record<string, any> };
    };

    const empty = await get('/api/assets/stores');
    assert.equal(empty.status, 200, JSON.stringify(empty.body));
    assert.deepEqual(empty.body, []);

    const added = await post('/api/assets/stores', { name: 'team', root: path.join(tmp.dir, 'bucket') });
    assert.equal(added.status, 200, JSON.stringify(added.body));
    assert.deepEqual(added.body.stores.map((s: { name: string }) => s.name), ['team']);
    // Bound to this machine, which is this server's own file and nothing to do with a folder.
    const bound = await post('/api/assets/stores/bind', { name: 'team', location: tmp.dir });
    assert.equal(bound.body.stores[0].bound_to, tmp.dir);

    const status = await get('/api/assets?prefix=/notes');
    assert.equal(status.status, 404);
    assert.match(status.body.message, /TEXTDB_SYNC/);
  });
});
