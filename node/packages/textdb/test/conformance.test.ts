import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { after, before, describe, test } from 'node:test';
import pg from 'pg';
import { type CorpusApi, Unsupported, connect } from '../src/index.ts';
import { type TempDir, tempStore } from './helpers.ts';

/**
 * One matrix, both engines.
 *
 * The SDK grew a Postgres client and the two backends answer the same questions through entirely
 * different SQL, so "it works" has to mean "it answers the same thing". That is what this file
 * asserts, and the traps it exists for are the quiet ones:
 *
 * - **`int8` arrives as a string** from node-postgres unless a type parser says otherwise, so
 *   every count, size and version would be a string and only arithmetic would notice. Hence the
 *   `typeof` assertions: they look pedantic and they are the point.
 * - **Timestamps.** `docs/shapes.md` fixes ISO-8601 UTC with milliseconds and `Z` on both
 *   backends, which Postgres only gives when the SQL says so.
 * - **Capabilities.** The trash is SQLite's alone, so Postgres must refuse it by saying so rather
 *   than by failing somewhere deep.
 *
 * Postgres runs when `TEXTDB_TEST_PG` names a cluster with the extension installed (CI's
 * `pg-extension` job); otherwise that half is skipped, as the Rust suites do.
 */

const PG_URL = process.env.TEXTDB_TEST_PG?.trim();

/** A store to run the matrix against, and how to throw it away afterwards. */
interface Target {
  name: string;
  open(): Promise<CorpusApi>;
  cleanup(): Promise<void>;
}

function sqliteTarget(): Target {
  let tmp: TempDir | undefined;
  return {
    name: 'sqlite',
    async open() {
      tmp = tempStore();
      return connect({ store: tmp.db, author: 'conformance' });
    },
    async cleanup() {
      tmp?.remove();
    },
  };
}

function postgresTarget(url: string): Target {
  const name = `textdb_sdk_${process.pid}_${randomBytes(4).toString('hex')}`;
  const base = url.slice(0, url.lastIndexOf('/'));
  return {
    name: 'postgres',
    async open() {
      const admin = new pg.Client({ connectionString: url });
      await admin.connect();
      try {
        await admin.query(`CREATE DATABASE ${name}`);
      } finally {
        await admin.end();
      }
      const fresh = new pg.Client({ connectionString: `${base}/${name}` });
      await fresh.connect();
      try {
        await fresh.query('CREATE EXTENSION textdb_pg');
      } finally {
        await fresh.end();
      }
      return connect({ store: `${base}/${name}`, author: 'conformance' });
    },
    async cleanup() {
      const admin = new pg.Client({ connectionString: url });
      await admin.connect();
      try {
        await admin.query(`DROP DATABASE IF EXISTS ${name} WITH (FORCE)`);
      } finally {
        await admin.end();
      }
    },
  };
}

const targets: Target[] = [sqliteTarget(), ...(PG_URL ? [postgresTarget(PG_URL)] : [])];

/** Every number in a record really is a number, not a string that looks like one. */
function numbers(row: Record<string, unknown>, keys: string[], where: string): void {
  for (const key of keys) {
    const value = row[key];
    if (value === null) continue;
    assert.equal(typeof value, 'number', `${where}: ${key} must be a number, got ${typeof value} (${String(value)})`);
  }
}

const ISO = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;

for (const target of targets) {
  describe(`conformance (${target.name})`, () => {
    let kb: CorpusApi;

    before(async () => {
      kb = await target.open();
      await kb.write('/notes/a.md', '# Alpha\n\nalpha beta\n\n## Deep\n\ngamma\n', { message: 'first' });
      await kb.write('/notes/b.md', '---\ntags: [x]\ntitle: Bee\n---\n\n# Bee\n\n[a](a.md) and [gone](nope.md)\n');
      await kb.write('/other/c.md', 'plain\n');
    });

    after(async () => {
      await kb.close();
      await target.cleanup();
    });

    test('says which backend it is, and what it can do', async () => {
      assert.equal(kb.backend, target.name);
      assert.equal(kb.capabilities.trash, target.name === 'sqlite');
      assert.equal(kb.capabilities.revertBatch, target.name === 'sqlite');
      // A Postgres store must say so rather than failing somewhere deep.
      if (!kb.capabilities.trash) await assert.rejects(async () => kb.trash(), (e: unknown) => e instanceof Unsupported);
    });

    test('a store URL never carries its password', async () => {
      const info = await kb.info();
      assert.equal(typeof info.files, 'number');
      assert.equal(info.files, 3);
      assert.equal(typeof info.last_seq, 'number');
      assert.ok(!info.db.includes('ci:ci'), `the password must not be in ${info.db}`);
    });

    test('a listing row is the canonical record, with numbers as numbers', async () => {
      const row = await kb.entry('/notes/a.md');
      numbers(
        row as unknown as Record<string, unknown>,
        ['version', 'nbytes', 'nlines', 'id', 'depth', 'nwords', 'nsections', 'nprops', 'nlinks', 'nlinks_broken', 'versions', 'nauthors'],
        'entry',
      );
      assert.match(row.updated_at, ISO, 'updated_at is ISO-8601 with milliseconds and Z');
      assert.match(row.created_at, ISO);
      assert.equal(row.kind, 'file');
      assert.equal(row.name, 'a.md');
      assert.equal(row.dir, '/notes');
      assert.equal(row.title, 'Alpha');
      assert.equal(row.nsections, 2);
      // The access tier: this is the owner, who reaches everything directly.
      assert.equal(row.share, null);
      assert.equal(row.rights, null);
      assert.deepEqual(row.shares, []);
      assert.deepEqual(row.authors, [
        { author: 'conformance', commits: 1, first_ts: row.authors[0]?.first_ts ?? '', last_ts: row.authors[0]?.last_ts ?? '' },
      ]);

      const folder = await kb.entry('/notes');
      assert.equal(folder.kind, 'folder');
      assert.equal(folder.version, null);
      assert.equal(folder.files, 2);
      numbers(folder as unknown as Record<string, unknown>, ['nbytes', 'nlines', 'files', 'folders', 'versions'], 'folder');
    });

    test('ls puts folders first, list sorts and pages', async () => {
      const top = await kb.ls('/');
      assert.deepEqual(
        top.map((e) => e.path),
        ['/notes', '/other'],
      );

      const page = await kb.list('/notes', { sort: 'name', limit: 1 });
      assert.equal(page.total, 2);
      assert.equal(page.limit, 1);
      assert.equal(page.entries.length, 1);
      assert.equal(page.entries[0]?.name, 'a.md');

      const second = await kb.list('/notes', { sort: 'name', limit: 1, offset: 1 });
      assert.equal(second.entries[0]?.name, 'b.md');

      const desc = await kb.list('/notes', { sort: 'size', order: 'desc' });
      assert.ok((desc.entries[0]?.nbytes ?? 0) >= (desc.entries[1]?.nbytes ?? 0));

      const byName = await kb.list('/', { name: 'b', recursive: true });
      assert.deepEqual(
        byName.entries.map((e) => e.path),
        ['/notes/b.md'],
      );

      const files = await kb.list('/', { kind: 'file', recursive: true });
      assert.equal(files.entries.every((e) => e.kind === 'file'), true);
      assert.equal(files.total, 3);

      const mine = await kb.list('/', { author: 'conformance', recursive: true });
      assert.equal(mine.total >= 3, true);
    });

    test('read gives head and an older version, with the version its lines belong to', async () => {
      const head = await kb.read('/notes/a.md');
      // Reading the head names the version it is, and says it is the head.
      assert.equal(head.version, head.head_version);
      assert.equal(head.head_version, 1);
      assert.match(head.content, /^# Alpha/);
      numbers(head as unknown as Record<string, unknown>, ['head_version', 'nbytes', 'nlines'], 'read');
      assert.match(head.updated_at, ISO);

      const next = await kb.write('/notes/a.md', '# Alpha\n\nalpha beta gamma\n');
      assert.equal(next.version, 2);
      assert.equal(next.kind, 'direct');
      const old = await kb.read('/notes/a.md', 1);
      assert.match(old.content, /## Deep/);
      assert.equal(old.head_version, 2);
      assert.match(old.updated_at, ISO);

      // An unchanged write is not a new version, on either engine.
      const again = await kb.write('/notes/a.md', '# Alpha\n\nalpha beta gamma\n');
      assert.equal(again.kind, 'noop');
      assert.equal(again.version, 2);
    });

    test('history, chunks, hunks and diff describe the same two versions', async () => {
      const history = await kb.history('/notes/a.md');
      assert.equal(history.length, 2);
      assert.equal(history[0]?.version, 1);
      assert.equal(history[0]?.author, 'conformance');
      assert.equal(history[0]?.message, 'first');
      assert.match(history[0]?.ts ?? '', ISO);
      numbers(history[0] as unknown as Record<string, unknown>, ['version', 'nbytes', 'nlines', 'nwords'], 'history');

      const chunks = await kb.chunks('/notes/a.md');
      assert.ok(chunks.length >= 1);
      numbers(chunks[0] as unknown as Record<string, unknown>, ['ord', 'byte_from', 'nbytes', 'line_from', 'nlines'], 'chunks');
      assert.equal(typeof chunks[0]?.hash, 'string');

      const hunks = await kb.hunks('/notes/a.md', 1, 2);
      assert.ok(hunks.length >= 1);
      numbers(hunks[0] as unknown as Record<string, unknown>, ['old_from', 'old_count', 'new_from', 'new_count'], 'hunks');

      const diff = await kb.diff('/notes/a.md', 1, 2);
      assert.match(diff, /^--- /m);
      assert.match(diff, /\+alpha beta gamma/);
    });

    test('search answers lines, with the version each line belongs to', async () => {
      const hits = await kb.search('alpha');
      assert.ok(hits.length >= 1);
      const hit = hits[0];
      assert.equal(hit?.path, '/notes/a.md');
      numbers(hit as unknown as Record<string, unknown>, ['version', 'line', 'score'], 'search');
      assert.equal(typeof hit?.text, 'string');
      assert.equal(typeof hit?.more, 'number');

      assert.deepEqual(await kb.search('nothinglikethis'), []);
    });

    test('links and backlinks give the canonical row, an unreachable one as broken', async () => {
      const out = await kb.links('/notes/b.md');
      const statuses = new Map(out.map((l) => [l.target, l.status]));
      assert.equal(statuses.get('a.md'), 'ok');
      assert.equal(statuses.get('nope.md'), 'broken');
      const ok = out.find((l) => l.target === 'a.md');
      assert.equal(ok?.resolved, '/notes/a.md');
      assert.equal(ok?.asset, false);
      numbers(ok as unknown as Record<string, unknown>, ['version', 'line'], 'links');

      const back = await kb.backlinks('/notes/a.md');
      assert.deepEqual(
        back.map((l) => l.path),
        ['/notes/b.md'],
      );
    });

    test('outlines and heading names read the markdown', async () => {
      const outline = await kb.outline('/notes/a.md');
      assert.deepEqual(
        outline.map((h) => h.heading),
        ['Alpha'],
        'version 2 of a.md has one heading',
      );
      numbers(outline[0] as unknown as Record<string, unknown>, ['level', 'lineFrom', 'lineTo', 'nwords', 'version'], 'outline');
      assert.match(outline[0]?.updated_at ?? '', ISO);

      const names = await kb.headingNames('/', { starts: 'B' });
      assert.deepEqual(
        names.map((n) => n.heading),
        ['Bee'],
      );
      numbers(names[0] as unknown as Record<string, unknown>, ['sections', 'docs'], 'headingNames');
    });

    test('front-matter properties are discoverable', async () => {
      const keys = await kb.propertyKeys();
      const names = keys.map((k) => k.key);
      for (const key of ['tags', 'title']) assert.ok(names.includes(key), `${key} is in ${names.join(', ')}`);
      numbers(keys[0] as unknown as Record<string, unknown>, ['docs', 'values_n'], 'propertyKeys');

      const values = await kb.propertyValues('tags');
      assert.deepEqual(
        values.map((v) => v.value),
        ['x'],
      );

      const found = await kb.propertyFind('tags:x');
      assert.deepEqual(
        found.map((f) => f.path),
        ['/notes/b.md'],
      );
      numbers(found[0] as unknown as Record<string, unknown>, ['nbytes'], 'propertyFind');
      assert.match(found[0]?.updated_at ?? '', ISO);
    });

    test('edit, append, move, remove and a bulk move all land', async () => {
      await kb.write('/work/x.md', 'one\ntwo\n');
      assert.equal(await kb.edit('/work/x.md', 'two', 'TWO'), 2);
      assert.equal(await kb.append('/work/x.md', 'three\n'), 3);
      assert.match((await kb.read('/work/x.md')).content, /TWO\nthree/);

      const replaced = await kb.replaceLines('/work/x.md', 1, 1, 'ONE\n');
      assert.equal(replaced.version, 4);
      assert.match((await kb.read('/work/x.md')).content, /^ONE/);

      await kb.move('/work/x.md', '/work/y.md');
      await assert.rejects(async () => kb.entry('/work/x.md'));
      assert.equal((await kb.entry('/work/y.md')).name, 'y.md');

      await kb.write('/work/z.md', 'zed\n');
      const bulk = await kb.bulk('move', ['/work/y.md', '/work/z.md'], { to: '/moved' });
      assert.deepEqual(bulk.done, ['/work/y.md', '/work/z.md']);
      assert.deepEqual(bulk.skipped, []);
      assert.equal((await kb.list('/moved')).total, 2);

      await kb.remove('/moved/z.md');
      await assert.rejects(async () => kb.entry('/moved/z.md'));
    });

    test('an import counts what it did and reports what the store refused', async () => {
      const stats = await kb.importBatch([
        { path: '/import/one.md', content: 'one\n' },
        { path: '/import/two.md', content: 'two\n' },
        // A file cannot hold a folder, so this one is refused while the two above still land.
        { path: '/import/one.md/nested.md', content: 'nope\n' },
      ]);
      assert.equal(stats.created, 2);
      assert.equal(stats.failed, 1);
      assert.equal(stats.failures[0]?.path, '/import/one.md/nested.md');
      // The two that were fine still landed: each write runs under its own savepoint.
      assert.equal((await kb.list('/import')).total, 2);

      const same = await kb.importBatch([{ path: '/import/one.md', content: 'one\n' }]);
      assert.equal(same.unchanged, 1);
    });

    test('export lists the files below a folder, and bytes come back as stored', async () => {
      const files = await kb.exportFiles('/notes');
      assert.deepEqual(
        files.map((f) => f.rel).sort(),
        ['a.md', 'b.md'],
      );
      numbers(files[0] as unknown as Record<string, unknown>, ['nbytes'], 'exportFiles');
      assert.match(files[0]?.updated_at ?? '', ISO);

      const bytes = await kb.readBytes('/notes/a.md');
      assert.ok(bytes instanceof Uint8Array);
      assert.match(new TextDecoder().decode(bytes), /^# Alpha/);
    });

    test('stat counts a folder, the feed records every change in order', async () => {
      const folder = await kb.stat('/notes');
      assert.equal(folder.kind, 'folder');
      assert.equal(folder.files, 2);
      numbers(folder as unknown as Record<string, unknown>, ['files', 'folders', 'nbytes'], 'stat');
      const file = await kb.stat('/notes/a.md');
      assert.deepEqual({ kind: file.kind, files: file.files, folders: file.folders }, { kind: 'file', files: 1, folders: 0 });

      const feed = await kb.feed(0, 5);
      assert.ok(feed.length >= 1);
      numbers(feed[0] as unknown as Record<string, unknown>, ['seq', 'version', 'base_version'], 'feed');
      assert.match(feed[0]?.ts ?? '', ISO);
      // The first change of all is the folder the first write had to make.
      assert.equal(feed[0]?.op, 'mkdir');
      assert.equal(feed[0]?.path, '/notes');
      assert.ok(
        feed.some((c) => c.path === '/notes/a.md'),
        'the first file is in the feed',
      );
      assert.ok((await kb.lastSeq()) >= (feed[0]?.seq ?? 0));
    });

    test('settings round-trip, and whoami is the owner', async () => {
      assert.equal(await kb.setSetting('path_history', 'off'), 'off');
      assert.equal(await kb.setting('path_history'), 'off');
      assert.equal(await kb.setSetting('path_history', null), null);

      assert.deepEqual(await kb.whoami(), { account: null, admin: true, kind: 'owner', namespace: 'store', shares: [] });
      assert.equal(kb.account, null);
    });
  });
}
