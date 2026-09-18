import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { after, before, describe, test } from 'node:test';
import { type RunningServer, startServer } from '../src/server.ts';
import { tempDir } from './helpers.ts';

let tmp: ReturnType<typeof tempDir>;
let server: RunningServer;

async function json(method: string, route: string, body?: unknown) {
  const res = await fetch(`${server.url}${route}`, {
    method,
    headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, body: await res.json() };
}

const CRLF = '﻿one\r\ntwo\r\n';

before(async () => {
  tmp = tempDir();
  server = await startServer({ store: path.join(tmp.dir, 'kb.db'), port: 0, webDist: path.join(tmp.dir, 'web'), pingMs: 100, watchIntervalMs: 20 });
  for (const [p, content] of [
    ['/repo/README.md', '# Readme\n'],
    ['/repo/docs/crlf.md', CRLF],
    ['/repo/docs/deep/ünicode.md', 'ü\n'],
    ['/repo0/outside.md', 'not below /repo\n'],
    ['/other.md', 'x\n'],
  ]) {
    assert.equal((await json('PUT', '/api/file', { path: p, content })).status, 200);
  }
});

after(async () => {
  await server.close();
  tmp.remove();
});

describe('export', () => {
  test('lists the files below a folder, and only those', async () => {
    const res = await json('GET', '/api/export/files?path=/repo');
    assert.equal(res.status, 200);
    assert.deepEqual(
      res.body.files.map((f: { rel: string; nbytes: number }) => [f.rel, f.nbytes]),
      [
        ['README.md', 9],
        ['docs/crlf.md', Buffer.byteLength(CRLF)],
        ['docs/deep/ünicode.md', 3],
      ],
    );
    const all = await json('GET', '/api/export/files');
    assert.equal(all.body.files.length, 5);
    assert.equal((await json('GET', '/api/export/files?path=/other.md')).status, 400);
  });

  test('serves stored bytes unchanged, and their hashes', async () => {
    const res = await fetch(`${server.url}/api/export/file?path=/repo/docs/crlf.md`);
    assert.equal(res.headers.get('content-type'), 'application/octet-stream');
    const bytes = Buffer.from(await res.arrayBuffer());
    assert.deepEqual(bytes, Buffer.from(CRLF));
    const hashes = await json('POST', '/api/export/hashes', { paths: ['/repo/docs/crlf.md'] });
    assert.deepEqual(hashes.body.hashes, [{ path: '/repo/docs/crlf.md', sha256: createHash('sha256').update(bytes).digest('hex') }]);
    assert.equal((await fetch(`${server.url}/api/export/file?path=/repo/missing.md`)).status, 404);
    assert.equal((await json('POST', '/api/export/hashes', { paths: 'nope' })).status, 400);
  });

  test('streams a zip of the folder', async () => {
    const res = await fetch(`${server.url}/api/export/zip?path=/repo`);
    assert.equal(res.status, 200);
    assert.equal(res.headers.get('content-type'), 'application/zip');
    assert.match(res.headers.get('content-disposition') ?? '', /filename="repo\.zip"/);
    const zip = Buffer.from(await res.arrayBuffer());
    assert.equal(zip.readUInt32LE(0), 0x04034b50);
    // Every entry named relative to the folder, in the central directory at the end.
    const names = [];
    for (let at = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x01, 0x02])); at >= 0; at = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x01, 0x02]), at - 1)) {
      const len = zip.readUInt16LE(at + 28);
      names.unshift(zip.subarray(at + 46, at + 46 + len).toString('utf8'));
    }
    assert.deepEqual(names, ['README.md', 'docs/crlf.md', 'docs/deep/ünicode.md']);
  });
});
