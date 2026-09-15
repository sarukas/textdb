import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { after, before, describe, test } from 'node:test';
import type { Change, Hunk } from '@textdb/node';
import { type RunningServer, startServer } from '../src/server.ts';
import { SseClient, tempDir, waitFor } from './helpers.ts';

type ChangeEvent = Change & { hunks?: Hunk[] };

let tmp: ReturnType<typeof tempDir>;
let server: RunningServer;

async function api(method: string, route: string, body?: unknown, headers: Record<string, string> = {}) {
  const res = await fetch(`${server.url}${route}`, {
    method,
    headers: body === undefined ? headers : { 'Content-Type': 'application/json', ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const type = res.headers.get('content-type') ?? '';
  return { status: res.status, headers: res.headers, body: type.includes('json') ? await res.json() : await res.text() };
}

/** A second connection writing to the store, as the Rust CLI would. */
function cli<T>(fn: (db: DatabaseSync) => T): T {
  const db = new DatabaseSync(server.corpus.db, { allowExtension: true });
  try {
    db.loadExtension(server.corpus.extension);
    db.exec('PRAGMA busy_timeout=30000');
    return fn(db);
  } finally {
    db.close();
  }
}

before(async () => {
  tmp = tempDir();
  const webDist = path.join(tmp.dir, 'web');
  mkdirSync(path.join(webDist, 'assets'), { recursive: true });
  writeFileSync(path.join(webDist, 'index.html'), '<!doctype html><title>textdb</title>');
  writeFileSync(path.join(webDist, 'assets', 'app.js'), 'console.log("app")');
  server = await startServer({ db: path.join(tmp.dir, 'kb.db'), port: 0, webDist, pingMs: 100, watchIntervalMs: 20 });
});

after(async () => {
  await server.close();
  tmp.remove();
});

describe('http api', () => {
  test('info on an empty store', async () => {
    const res = await api('GET', '/api/info');
    assert.equal(res.status, 200);
    assert.deepEqual(res.body, { db: server.corpus.db, files: 0, last_seq: 0 });
  });

  test('write, read, list and inspect a file', async () => {
    let res = await api('PUT', '/api/file', { path: '/guides/intro.md', content: 'one\ntwo\nthree\n', author: 'human', message: 'first' });
    assert.deepEqual([res.status, res.body], [200, { version: 1, kind: 'direct' }]);
    await api('PUT', '/api/file', { path: '/guides/b.md', content: 'b\n' });
    await api('PUT', '/api/file', { path: '/guides/deep/c.md', content: 'c\n' });

    res = await api('POST', '/api/replace-lines', { path: '/guides/intro.md', from: 2, to: 2, text: 'TWO\n', base_version: 1, author: 'agent-7' });
    assert.deepEqual(res.body, { version: 2, kind: 'direct' });

    res = await api('GET', '/api/file?path=/guides/intro.md');
    assert.equal(res.status, 200);
    const { updated_at, ...file } = res.body;
    assert.match(updated_at, /^\d{4}-\d\d-\d\dT/);
    assert.deepEqual(file, {
      path: '/guides/intro.md',
      version: 2,
      head_version: 2,
      content: 'one\nTWO\nthree\n',
      nbytes: 14,
      nlines: 3,
      updated_by: 'agent-7',
    });
    res = await api('GET', '/api/file?path=/guides/intro.md&version=1');
    assert.deepEqual([res.body.version, res.body.head_version, res.body.content, res.body.updated_by], [1, 2, 'one\ntwo\nthree\n', 'human']);

    res = await api('GET', '/api/ls?path=/guides');
    assert.deepEqual(
      res.body.map((e: { name: string; kind: string; path: string }) => [e.name, e.kind, e.path]),
      [
        ['deep', 'folder', '/guides/deep'],
        ['b.md', 'file', '/guides/b.md'],
        ['intro.md', 'file', '/guides/intro.md'],
      ],
    );
    res = await api('GET', '/api/ls');
    assert.deepEqual(res.body.map((e: { name: string }) => e.name), ['guides']);

    res = await api('GET', '/api/list?path=/guides&sort=size&order=desc&limit=2');
    assert.equal(res.status, 200);
    assert.deepEqual(
      [res.body.total, res.body.offset, res.body.entries.map((e: { name: string }) => e.name)],
      [3, 0, ['deep', 'intro.md']],
    );
    assert.deepEqual([res.body.entries[1].nwords, res.body.entries[1].versions], [3, 2]);
    res = await api('GET', '/api/list?path=/guides&sort=colour');
    assert.equal(res.status, 400);

    res = await api('GET', '/api/chunks?path=/guides/intro.md&version=1');
    assert.deepEqual(Object.keys(res.body[0]), ['ord', 'hash', 'byte_from', 'nbytes', 'line_from', 'nlines']);

    res = await api('GET', '/api/history?path=/guides/intro.md');
    assert.deepEqual(
      res.body.map((h: { version: number; author: string; kind: string; base_version: number | null }) => [h.version, h.author, h.kind, h.base_version]),
      [
        [1, 'human', 'direct', null],
        [2, 'agent-7', 'direct', 1],
      ],
    );

    const hunk = { old_from: 2, old_count: 1, new_from: 2, new_count: 1, old_text: 'two\n', new_text: 'TWO\n' };
    res = await api('GET', '/api/hunks?path=/guides/intro.md&from=1&to=2');
    assert.deepEqual(res.body, [hunk]);
    res = await api('GET', '/api/hunks?path=/guides/intro.md');
    assert.deepEqual(res.body, [hunk]);

    res = await api('GET', '/api/diff?path=/guides/intro.md&from=1&to=2');
    assert.match(res.body.diff, /^--- \/guides\/intro\.md@1\n\+\+\+ \/guides\/intro\.md@2\n@@ -2,1 \+2,1 @@\n-two\n\+TWO\n$/);

    res = await api('GET', '/api/search?q=TWO&prefix=/guides&limit=5');
    assert.deepEqual(res.body.map((h: { path: string; line: number }) => [h.path, h.line]), [['/guides/intro.md', 2]]);

    res = await api('GET', '/api/info');
    assert.equal(res.body.files, 3);
  });

  test('errors carry the store code and status', async () => {
    await api('PUT', '/api/file', { path: '/conflict.md', content: 'a\nb\nc\n' });
    await api('POST', '/api/replace-lines', { path: '/conflict.md', from: 2, to: 2, text: 'theirs\n', base_version: 1 });
    let res = await api('PUT', '/api/file', { path: '/conflict.md', content: 'a\nours\nc\n', base_version: 1 });
    assert.equal(res.status, 409);
    assert.deepEqual(res.body, {
      code: 'TX001',
      message: 'conflict',
      conflict: { path: '/conflict.md', region_line_from: 2, region_line_to: 2, base: 'b\n', theirs: 'theirs\n', ours: 'ours\n', current_version: 2 },
    });

    res = await api('GET', '/api/file?path=/missing.md');
    assert.deepEqual([res.status, res.body.code], [404, 'TX003']);
    res = await api('POST', '/api/replace-lines', { path: '/conflict.md', from: 9, to: 9, text: 'x\n' });
    assert.deepEqual([res.status, res.body.code], [400, 'TX004']);
    res = await api('GET', '/api/file');
    assert.deepEqual(res.body, { code: 'TX004', message: 'missing query parameter: path' });
    res = await api('GET', '/api/file?path=/conflict.md&version=two');
    assert.equal(res.status, 400);
    res = await api('PUT', '/api/file', { path: '/x.md' });
    assert.deepEqual(res.body, { code: 'TX004', message: 'content must be a string' });
    res = await api('GET', '/api/nope');
    assert.deepEqual([res.status, res.body.code], [404, 'TX003']);
  });

  test('CORS allows local origins only', async () => {
    let res = await api('OPTIONS', '/api/file', undefined, { Origin: 'http://localhost:5173', 'Access-Control-Request-Method': 'PUT' });
    assert.equal(res.headers.get('access-control-allow-origin'), 'http://localhost:5173');
    res = await api('GET', '/api/info', undefined, { Origin: 'https://example.com' });
    assert.equal(res.headers.get('access-control-allow-origin'), null);
  });

  test('serves the web build with an SPA fallback', async () => {
    let res = await api('GET', '/');
    assert.deepEqual([res.status, res.body], [200, '<!doctype html><title>textdb</title>']);
    res = await api('GET', '/files/guides/intro.md/history');
    assert.equal(res.headers.get('content-type'), 'text/html; charset=utf-8');
    res = await api('GET', '/assets/app.js');
    assert.deepEqual([res.body, res.headers.get('content-type')], ['console.log("app")', 'text/javascript; charset=utf-8']);
    res = await api('GET', '/assets/missing.js');
    assert.equal(res.status, 404);
  });

  test('imports a batch of files in one request', async () => {
    const files = [
      { path: '/imported/a.md', content: '# A\n' },
      { path: '/imported/deep/b.md', content: '# B\n' },
      { path: '/imported/../escape.md', content: 'no\n' },
    ];
    let res = await api('POST', '/api/import', { author: 'human', files });
    assert.equal(res.status, 200);
    assert.deepEqual([res.body.created, res.body.updated, res.body.unchanged, res.body.failed], [2, 0, 0, 1]);
    assert.deepEqual([res.body.failures[0].path, res.body.failures[0].code], ['/imported/../escape.md', 'TX004']);

    // Importing again is idempotent, and a changed file becomes a new version.
    res = await api('POST', '/api/import', { author: 'human', files: [files[0], { ...files[1]!, content: '# B, revised\n' }] });
    assert.deepEqual([res.body.created, res.body.updated, res.body.unchanged, res.body.failed], [0, 1, 1, 0]);
    res = await api('GET', '/api/history?path=/imported/deep/b.md');
    assert.deepEqual(res.body.map((h: { version: number; author: string; message: string }) => [h.version, h.author, h.message]), [
      [1, 'human', 'import'],
      [2, 'human', 'import'],
    ]);

    res = await api('GET', '/api/stat?path=/imported');
    assert.deepEqual(res.body, { path: '/imported', kind: 'folder', files: 2, folders: 1, nbytes: 4 + 13 });

    res = await api('POST', '/api/import', { files: [] });
    assert.deepEqual([res.status, res.body.code], [400, 'TX004']);
    res = await api('POST', '/api/import', { files: [{ path: '/x.md' }] });
    assert.deepEqual([res.status, res.body.message], [400, 'files[0] must be an object with string path and content']);
  });
});

describe('move and delete', () => {
  test('renames a file and moves a folder with everything below it, attributed', async () => {
    const since = server.corpus.lastSeq();
    for (const p of ['/tree/a.md', '/tree/sub/b.md', '/tree/sub/deep/c.md']) {
      await api('PUT', '/api/file', { path: p, content: `${p}\n` });
    }
    let res = await api('GET', '/api/stat?path=/tree');
    assert.deepEqual([res.body.kind, res.body.files, res.body.folders], ['folder', 3, 2]);
    res = await api('GET', '/api/stat?path=/tree/a.md');
    assert.deepEqual(res.body, { path: '/tree/a.md', kind: 'file', files: 1, folders: 0, nbytes: 11 });

    res = await api('POST', '/api/move', { from: '/tree/a.md', to: '/tree/renamed.md', author: 'human' });
    assert.deepEqual([res.status, res.body], [200, { from: '/tree/a.md', to: '/tree/renamed.md' }]);
    res = await api('POST', '/api/move', { from: '/tree/sub', to: '/elsewhere/sub2', author: 'human' });
    assert.equal(res.status, 200);
    res = await api('GET', '/api/file?path=/elsewhere/sub2/deep/c.md');
    assert.deepEqual([res.status, res.body.content, res.body.version], [200, '/tree/sub/deep/c.md\n', 1]);
    res = await api('GET', '/api/history?path=/elsewhere/sub2/deep/c.md');
    assert.equal(res.body.length, 1);

    res = await api('POST', '/api/move', { from: '/tree/renamed.md', to: '/elsewhere/sub2/b.md' });
    assert.deepEqual([res.status, res.body.code], [400, 'TX004']);
    res = await api('POST', '/api/move', { from: '/elsewhere', to: '/elsewhere/inside' });
    assert.deepEqual([res.status, res.body.code], [400, 'TX004']);
    res = await api('POST', '/api/move', { from: '/nope.md', to: '/x.md' });
    assert.deepEqual([res.status, res.body.code], [404, 'TX003']);

    res = await api('POST', '/api/delete', { path: '/elsewhere', author: 'agent-7' });
    assert.deepEqual([res.status, res.body], [200, { path: '/elsewhere' }]);
    res = await api('GET', '/api/file?path=/elsewhere/sub2/b.md');
    assert.equal(res.status, 404);
    res = await api('GET', '/api/stat?path=/elsewhere');
    assert.equal(res.status, 404);
    res = await api('POST', '/api/delete', { path: '/' });
    assert.deepEqual([res.status, res.body.code], [400, 'TX004']);

    res = await api('GET', '/api/path-history?path=/elsewhere/sub2/deep/c.md');
    assert.deepEqual(
      res.body.map((e: { op: string; old_path: string; new_path: string | null; via: string | null; author: string }) => [
        e.op,
        e.old_path,
        e.new_path,
        e.via,
        e.author,
      ]),
      [
        ['move', '/tree/sub/deep/c.md', '/elsewhere/sub2/deep/c.md', '/tree/sub', 'human'],
        ['delete', '/elsewhere/sub2/deep/c.md', null, '/elsewhere', 'agent-7'],
      ],
    );
    res = await api('GET', '/api/path-history?path=/tree/a.md');
    assert.equal(res.status, 404, 'renamed away, so nothing is at the old path');
    res = await api('GET', '/api/path-history?path=/tree/renamed.md');
    assert.deepEqual(
      res.body.map((e: { op: string; old_path: string }) => [e.op, e.old_path]),
      [['rename', '/tree/a.md']],
    );

    res = await api('GET', '/api/setting?key=path_history');
    assert.deepEqual(res.body, { key: 'path_history', value: null });
    res = await api('PUT', '/api/setting', { key: 'path_history', value: 'off' });
    assert.deepEqual(res.body, { key: 'path_history', value: 'off' });
    res = await api('PUT', '/api/setting', { key: 'path_history', value: null });
    assert.deepEqual(res.body, { key: 'path_history', value: null });
    res = await api('PUT', '/api/setting', { key: 'colour', value: 'blue' });
    assert.deepEqual([res.status, res.body.code], [400, 'TX004']);

    const moves = server.corpus
      .feed(since)
      .filter((c) => c.op === 'move' || c.op === 'delete')
      .map((c) => [c.op, c.path, c.old_path, c.node_kind, c.author]);
    assert.deepEqual(moves, [
      ['move', '/tree/renamed.md', '/tree/a.md', 'file', 'human'],
      ['move', '/elsewhere/sub2', '/tree/sub', 'folder', 'human'],
      ['delete', '/elsewhere', null, 'folder', 'agent-7'],
    ]);
  });
});

describe('trash', () => {
  test('lists, reads and purges what deletes left', async () => {
    await api('PUT', '/api/file', { path: '/bin/a.md', content: 'one\n' });
    await api('PUT', '/api/file', { path: '/bin/a.md', content: 'two\n' });
    await api('PUT', '/api/file', { path: '/bin/sub/b.md', content: 'b\n' });
    await api('POST', '/api/delete', { path: '/bin', author: 'human' });

    let res = await api('GET', '/api/trash');
    assert.equal(res.body[0].path, '/bin', 'newest delete first');
    const item = res.body[0];
    assert.deepEqual([item.kind, item.files, item.nbytes, item.deleted_by], ['folder', 2, 6, 'human']);
    res = await api('GET', `/api/trash?parent=${item.id}`);
    assert.deepEqual(res.body.map((e: { name: string }) => e.name), ['sub', 'a.md']);
    const file = res.body[1];

    res = await api('GET', `/api/trash/file?id=${file.id}`);
    assert.deepEqual([res.body.entry.path, res.body.entry.deleted_by, res.body.version, res.body.content], ['/bin/a.md', 'human', 2, 'two\n']);
    res = await api('GET', `/api/trash/file?id=${file.id}&version=1`);
    assert.deepEqual([res.body.version, res.body.content], [1, 'one\n']);
    res = await api('GET', `/api/trash/history?id=${file.id}`);
    assert.deepEqual(res.body.map((h: { version: number }) => h.version), [1, 2]);
    res = await api('GET', `/api/trash/file?id=${item.id}`);
    assert.deepEqual([res.status, res.body.code], [400, 'TX004']);

    res = await api('POST', '/api/trash/purge', { id: item.id, author: 'human' });
    assert.deepEqual([res.status, res.body.items, res.body.files, res.body.folders, res.body.versions], [200, 1, 2, 2, 3]);
    res = await api('GET', `/api/trash/file?id=${file.id}`);
    assert.deepEqual([res.status, res.body.code], [404, 'TX003']);

    res = await api('POST', '/api/trash/empty', { author: 'ops' });
    assert.equal(res.status, 200);
    res = await api('GET', '/api/trash');
    assert.deepEqual(res.body, []);
    const purges = server.corpus
      .feed(0)
      .filter((c) => (c.op as string) === 'purge')
      .map((c) => [c.path, c.author]);
    assert.deepEqual(purges[0], ['/bin', 'human']);
    assert.ok(purges.slice(1).every(([, author]) => author === 'ops'));
  });
});

describe('event stream', () => {
  test('a commit from another connection arrives with its hunks', async () => {
    await api('PUT', '/api/file', { path: '/live.md', content: 'alpha\nbeta\ngamma\n' });
    const client = await SseClient.connect(`${server.url}/api/events`);
    try {
      await waitFor(() => (client.comments.includes('connected') ? true : undefined));
      const result = cli((db) => db.prepare("SELECT textdb_replace_lines('/live.md', 2, 2, 'BETA' || char(10) || 'beta2' || char(10), 1, 'agent-7') AS r").get());
      assert.deepEqual(JSON.parse(String(result?.r)), { version: 2, kind: 'direct' });

      const event = await waitFor(() => client.changes<ChangeEvent>().find((c) => c.path === '/live.md'));
      assert.deepEqual(
        [event.op, event.version, event.base_version, event.commit_kind, event.author, event.message],
        ['commit', 2, 1, 'direct', 'agent-7', 'replace-lines'],
      );
      assert.deepEqual(event.hunks, [{ old_from: 2, old_count: 1, new_from: 2, new_count: 2, old_text: 'beta\n', new_text: 'BETA\nbeta2\n' }]);
      assert.equal(client.events.at(-1)?.id, String(event.seq));
      await waitFor(() => (client.comments.includes('ping') ? true : undefined));
    } finally {
      await client.close();
    }
  });

  test('hunks above 256 KiB are left out', async () => {
    const big = `${'x'.repeat(99)}\n`.repeat(3000);
    await api('PUT', '/api/file', { path: '/big.md', content: 'small\n' });
    const client = await SseClient.connect(`${server.url}/api/events`);
    try {
      await waitFor(() => (client.comments.includes('connected') ? true : undefined));
      await api('PUT', '/api/file', { path: '/big.md', content: big });
      const event = await waitFor(() => client.changes<ChangeEvent>().find((c) => c.path === '/big.md'));
      assert.equal(event.op, 'commit');
      assert.equal('hunks' in event, false);
    } finally {
      await client.close();
    }
  });

  test('since replays the backlog and joins the live stream without gaps or duplicates', async () => {
    const since = server.corpus.lastSeq();
    for (let i = 0; i < 5; i++) await api('PUT', '/api/file', { path: `/replay/${i}.md`, content: `${i}\n` });

    // Keep committing from another process-like connection while the client connects.
    let n = 0;
    const writer = setInterval(() => cli((db) => db.prepare('SELECT textdb_write(?, ?)').get('/replay/live.md', `${n++}\n`)), 5);
    const client = await SseClient.connect(`${server.url}/api/events?since=${since}`);
    try {
      await new Promise((resolve) => setTimeout(resolve, 300));
      // A slow machine may fit only a few writes in that time: the check below needs some.
      await waitFor(() => (n >= 6 ? true : undefined));
      clearInterval(writer);
      const expected = server.corpus.feed(since).map((c) => c.seq);
      await waitFor(() => (client.changes().length >= expected.length ? true : undefined));
      await new Promise((resolve) => setTimeout(resolve, 100));
      assert.deepEqual(client.changes<ChangeEvent>().map((c) => c.seq), expected);
      assert.ok(expected.length > 10, `only ${expected.length} changes`);
    } finally {
      clearInterval(writer);
      await client.close();
    }
  });

  test('Last-Event-ID takes precedence over since', async () => {
    const last = server.corpus.lastSeq();
    await api('PUT', '/api/file', { path: '/resume.md', content: 'one\n' });
    await api('PUT', '/api/file', { path: '/resume.md', content: 'two\n' });
    const client = await SseClient.connect(`${server.url}/api/events?since=0`, { 'Last-Event-ID': String(last + 1) });
    try {
      await waitFor(() => (client.changes().length > 0 ? true : undefined));
      await new Promise((resolve) => setTimeout(resolve, 100));
      assert.deepEqual(client.changes<ChangeEvent>().map((c) => [c.seq, c.op]), [[last + 2, 'commit']]);
    } finally {
      await client.close();
    }
  });

  test('a disconnected client is unsubscribed', async () => {
    const client = await SseClient.connect(`${server.url}/api/events`);
    await waitFor(() => (server.hub.subscriberCount === 1 ? true : undefined));
    await client.close();
    await waitFor(() => (server.hub.subscriberCount === 0 ? true : undefined));
  });
});
