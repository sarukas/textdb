import assert from 'node:assert/strict';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { after, before, describe, test } from 'node:test';
import { parseSyncLinks } from '../src/config.ts';
import { resolveMarkers } from '../src/markers.ts';
import { type RunningServer, startServer } from '../src/server.ts';
import { findCli } from '../src/sync.ts';
import { tempDir } from './helpers.ts';

describe('sync configuration and conflict markers', () => {
  test('TEXTDB_SYNC names folders and directories', () => {
    const links = parseSyncLinks('/handbook=./handbook ; /notes/=/srv/notes\n');
    assert.deepEqual(
      links.map((l) => [l.prefix, path.isAbsolute(l.dir)]),
      [
        ['/handbook', true],
        ['/notes', true],
      ],
    );
    assert.throws(() => parseSyncLinks('handbook=./handbook'), /expected \/folder=directory/);
  });

  test('keeping one side of each conflict leaves the merged text around it', () => {
    const text = 'a\n<<<<<<< textdb\nours\n=======\ntheirs\n>>>>>>> disk\nb\r\n<<<<<<< textdb\r\nx\r\n=======\r\ny\r\n>>>>>>> disk\r\n';
    assert.equal(resolveMarkers(text, 'textdb'), 'a\nours\nb\r\nx\r\n');
    assert.equal(resolveMarkers(text, 'disk'), 'a\ntheirs\nb\r\ny\r\n');
    assert.throws(() => resolveMarkers('<<<<<<< textdb\nno end\n', 'disk'), /without its end/);
  });
});

const cli = findCli(process.env.TEXTDB_CLI);

describe('sync with a directory', { skip: cli ? false : 'the textdb CLI is not built (or set TEXTDB_CLI)' }, () => {
  let tmp: ReturnType<typeof tempDir>;
  let server: RunningServer;
  let notes: string;

  async function call(method: string, route: string, body?: unknown) {
    const res = await fetch(`${server.url}${route}`, {
      method,
      headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: await res.json() };
  }

  before(async () => {
    tmp = tempDir();
    notes = path.join(tmp.dir, 'notes');
    mkdirSync(notes);
    writeFileSync(path.join(notes, 'a.md'), 'one\ntwo\nthree\n');
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
  });

  test('plans, syncs, reports a conflict and resolves it', async () => {
    let links = await call('GET', '/api/sync/links');
    assert.equal(links.body.available, true);
    assert.deepEqual(
      links.body.links.map((l: { prefix: string; last: unknown }) => [l.prefix, l.last]),
      [['/notes', null]],
    );

    let res = await call('POST', '/api/sync', { prefix: '/notes', dry_run: true });
    assert.equal(res.status, 200, JSON.stringify(res.body));
    assert.deepEqual(res.body.to_textdb.new, ['a.md']);
    assert.equal((await call('GET', '/api/file?path=/notes/a.md')).status, 404);

    res = await call('POST', '/api/sync', { prefix: '/notes', author: 'web' });
    assert.deepEqual(res.body.to_textdb.new, ['a.md']);
    assert.equal((await call('GET', '/api/file?path=/notes/a.md')).body.content, 'one\ntwo\nthree\n');

    // The same line changed in the store and on disk.
    await call('PUT', '/api/file', { path: '/notes/a.md', content: 'one\nTWO (textdb)\nthree\n', author: 'agent' });
    writeFileSync(path.join(notes, 'a.md'), 'one\nTWO (disk)\nthree\n');
    res = await call('POST', '/api/sync', { prefix: '/notes' });
    assert.deepEqual(res.body.conflicts, ['a.md']);
    links = await call('GET', '/api/sync/links');
    assert.deepEqual(links.body.links[0].last.conflicts, ['a.md']);

    res = await call('GET', '/api/sync/conflict?prefix=/notes&rel=a.md');
    assert.equal(res.body.text, 'one\n<<<<<<< textdb\nTWO (textdb)\n=======\nTWO (disk)\n>>>>>>> disk\nthree\n');
    assert.equal((await call('GET', '/api/sync/conflict?prefix=/notes&rel=../kb.db')).status, 404);

    res = await call('POST', '/api/sync/resolve', { prefix: '/notes', rel: 'a.md', keep: 'disk', author: 'web' });
    assert.equal(res.status, 200, JSON.stringify(res.body));
    assert.deepEqual(res.body.to_textdb.changed, ['a.md']);
    assert.equal(readFileSync(path.join(notes, 'a.md'), 'utf8'), 'one\nTWO (disk)\nthree\n');
    assert.equal((await call('GET', '/api/file?path=/notes/a.md')).body.content, 'one\nTWO (disk)\nthree\n');
    links = await call('GET', '/api/sync/links');
    assert.deepEqual([links.body.links[0].last.conflicts, links.body.links[0].last.changed], [[], 0]);

    await call('PUT', '/api/file', { path: '/notes/b.md', content: 'new\n' });
    links = await call('GET', '/api/sync/links');
    assert.equal(links.body.links[0].last.changed, 1);

    assert.equal((await call('POST', '/api/sync', { prefix: '/elsewhere' })).status, 404);
    assert.equal((await call('POST', '/api/sync', { prefix: '/notes', base: '--upload-pack=x' })).status, 400);
  });
});
