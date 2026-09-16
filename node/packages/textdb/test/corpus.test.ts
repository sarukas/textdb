import assert from 'node:assert/strict';
import { after, before, describe, test } from 'node:test';
import { Conflict, type Corpus, InvalidEdit, NotFound, TextdbError, fromMessage, openCorpus } from '../src/index.ts';
import { type TempDir, tempStore } from './helpers.ts';

describe('corpus', () => {
  let tmp: TempDir;
  let kb: Corpus;

  before(() => {
    tmp = tempStore();
    kb = openCorpus({ db: tmp.db, author: 'tester' });
  });

  after(() => {
    kb.close();
    tmp.remove();
  });

  test('opens and migrates an empty store, and reopens it', () => {
    assert.deepEqual(kb.info(), { db: tmp.db, files: 0, last_seq: 0 });
    const again = openCorpus({ db: tmp.db });
    assert.equal(again.lastSeq(), 0);
    again.close();
  });

  test('write, read and history', () => {
    assert.deepEqual(kb.write('/docs/a.md', 'one\ntwo\n', { message: 'first' }), { version: 1, kind: 'direct' });
    assert.deepEqual(kb.write('/docs/a.md', 'one\ntwo\n'), { version: 1, kind: 'noop' });
    assert.deepEqual(kb.write('/docs/a.md', 'one\nTWO\n', { baseVersion: 1, author: 'human' }), {
      version: 2,
      kind: 'direct',
    });

    const head = kb.read('/docs/a.md');
    assert.equal(head.content, 'one\nTWO\n');
    assert.deepEqual([head.version, head.head_version, head.nbytes, head.nlines, head.updated_by], [2, 2, 8, 2, 'human']);
    const v1 = kb.read('/docs/a.md', 1);
    assert.deepEqual([v1.content, v1.version, v1.head_version, v1.updated_by], ['one\ntwo\n', 1, 2, 'tester']);

    const history = kb.history('/docs/a.md');
    assert.deepEqual(
      history.map((h) => [h.version, h.author, h.message, h.kind, h.base_version]),
      [
        [1, 'tester', 'first', 'direct', null],
        [2, 'human', null, 'direct', 1],
      ],
    );
    // The nine keys of the history row, in the order the `commits` view uses on both engines.
    assert.deepEqual(Object.keys(history[0]), [
      'version',
      'author',
      'ts',
      'message',
      'kind',
      'base_version',
      'nbytes',
      'nlines',
      'nwords',
    ]);
    assert.deepEqual(kb.hunks('/docs/a.md'), [
      { old_from: 2, old_count: 1, new_from: 2, new_count: 1, old_text: 'two\n', new_text: 'TWO\n' },
    ]);
    assert.deepEqual(kb.hunks('/docs/a.md', 1, 2), kb.hunks('/docs/a.md'));
    assert.match(kb.diff('/docs/a.md', 1, 2), /-two\n\+TWO\n/);
    assert.equal(kb.chunks('/docs/a.md').length, 1);
    assert.deepEqual(kb.search('TWO').map((h) => [h.path, h.line]), [['/docs/a.md', 2]]);
    assert.equal(kb.append('/docs/a.md', 'three\n'), 3);
    assert.equal(kb.edit('/docs/a.md', 'three', 'THREE'), 4);
    assert.deepEqual(kb.ls('/').map((e) => [e.name, e.kind]), [['docs', 'folder']]);
  });

  test('ls lists folders before files', () => {
    kb.write('/mix/b.md', 'b\n');
    kb.write('/mix/z/inner.md', 'z\n');
    kb.write('/mix/a.md', 'a\n');
    assert.deepEqual(kb.ls('/mix').map((e) => e.name), ['z', 'a.md', 'b.md']);
  });

  test('a stale base_version on the same lines raises a Conflict', () => {
    kb.write('/c.md', 'a\nb\nc\n');
    kb.replaceLines('/c.md', 2, 2, 'theirs\n', { baseVersion: 1 });
    assert.throws(
      () => kb.replaceLines('/c.md', 2, 2, 'ours\n', { baseVersion: 1 }),
      (error: unknown) => {
        assert.ok(error instanceof Conflict);
        assert.equal(error.code, 'TX001');
        assert.equal(error.message, 'conflict');
        assert.deepEqual(error.payload, {
          path: '/c.md',
          region_line_from: 2,
          region_line_to: 2,
          base: 'b\n',
          theirs: 'theirs\n',
          ours: 'ours\n',
          current_version: 2,
        });
        return true;
      },
    );
  });

  test('errors are typed by code', () => {
    assert.throws(() => kb.read('/missing.md'), NotFound);
    assert.throws(() => kb.history('/missing.md'), NotFound);
    assert.throws(() => kb.edit('/c.md', 'no such text', 'x'), InvalidEdit);
    assert.throws(() => kb.move('/missing.md', '/elsewhere.md'), NotFound);
    const contention = fromMessage('TX002 contention: retry budget exhausted');
    assert.deepEqual([contention?.name, contention?.code, contention?.message], ['Contention', 'TX002', 'contention: retry budget exhausted']);
    assert.equal(fromMessage('no such table: kb'), undefined);
    assert.ok(fromMessage('TX000 internal') instanceof TextdbError);
  });

  test('the feed records every change in order', () => {
    const since = kb.lastSeq();
    kb.write('/feed/a.md', 'x\n', { author: 'alice' });
    kb.write('/feed/a.md', 'y\n', { author: 'bob' });
    kb.move('/feed/a.md', '/moved/a.md', { author: 'carol' });
    kb.remove('/moved', { author: 'dave' });

    assert.deepEqual(
      kb.pathHistory('/moved/a.md').map((e) => [e.op, e.old_path, e.new_path, e.via, e.version, e.author]),
      [
        ['move', '/feed/a.md', '/moved/a.md', null, 2, 'carol'],
        ['delete', '/moved/a.md', null, '/moved', 2, 'dave'],
      ],
    );
    const [first] = kb.pathHistory('/moved/a.md');
    assert.equal(kb.pathHistoryOf(kb.trash(Number(kb.trash()[0]!.id))[0]!.id).length, 2);
    assert.match(first!.ts, /^\d{4}-\d\d-\d\dT/);
    assert.equal(kb.setting('path_history'), null);
    assert.equal(kb.setSetting('path_history', 'off'), 'off');
    assert.equal(kb.setSetting('path_history', null), null);
    assert.throws(() => kb.setting('colour'), InvalidEdit);

    const rows = kb.feed(since);
    assert.deepEqual(
      rows.map((r) => [r.op, r.path, r.old_path, r.node_kind, r.version, r.base_version, r.commit_kind, r.author]),
      [
        ['mkdir', '/feed', null, 'folder', null, null, null, null],
        ['create', '/feed/a.md', null, 'file', 1, null, 'direct', 'alice'],
        ['commit', '/feed/a.md', null, 'file', 2, 1, 'direct', 'bob'],
        ['mkdir', '/moved', null, 'folder', null, null, null, null],
        ['move', '/moved/a.md', '/feed/a.md', 'file', null, null, null, 'carol'],
        ['delete', '/moved', null, 'folder', null, null, null, 'dave'],
      ],
    );
    assert.ok(rows.every((r, i) => i === 0 || r.seq > rows[i - 1]!.seq));
    assert.equal(rows.at(-1)!.seq, kb.lastSeq());
    assert.equal(kb.feed(since, 2).length, 2);
  });

  test('transaction commits atomically and rolls back on error', () => {
    kb.transaction(() => {
      kb.write('/tx/1.md', '1\n');
      kb.write('/tx/2.md', '2\n');
    });
    assert.equal(kb.ls('/tx').length, 2);
    assert.throws(() =>
      kb.transaction(() => {
        kb.write('/tx/3.md', '3\n');
        throw new Error('abort');
      }),
    );
    assert.throws(() => kb.read('/tx/3.md'), NotFound);
  });

  test('list sorts, filters and pages with words, authors and folder totals', () => {
    kb.write('/lst/b.md', 'alpha beta gamma\n', { author: 'ann' });
    kb.write('/lst/b.md', 'alpha beta gamma\ndelta\n', { author: 'bo' });
    kb.write('/lst/a.txt', 'one\n', { author: 'ann' });
    kb.write('/lst/sub/c.md', 'x y\n', { author: 'cy' });

    const page = kb.list('/lst', { sort: 'words', order: 'desc' });
    assert.equal(page.total, 3);
    assert.deepEqual(
      page.entries.map((e) => [e.name, e.nwords]),
      [
        ['sub', 2],
        ['b.md', 4],
        ['a.txt', 1],
      ],
    );
    const sub = page.entries[0]!;
    const b = page.entries[1]!;
    assert.deepEqual([sub.kind, sub.files, sub.folders, sub.nbytes, sub.versions, sub.authors], ['folder', 1, 0, 4, 1, []]);
    assert.deepEqual([b.versions, b.nauthors, b.authors.map((a) => a.author).sort()], [2, 2, ['ann', 'bo']]);

    assert.deepEqual(kb.list('/lst', { type: '.md' }).entries.map((e) => e.name), ['b.md']);
    assert.deepEqual(
      kb.list('/lst', { recursive: true, name: '*.md' }).entries.map((e) => e.path),
      ['/lst/b.md', '/lst/sub/c.md'],
    );
    assert.deepEqual(kb.list('/lst', { recursive: true, author: 'cy' }).entries.map((e) => e.path), ['/lst/sub/c.md']);
    assert.deepEqual(kb.list('/lst', { name: '50%' }).total, 0);

    const last = kb.list('/lst', { limit: 2, offset: 2 });
    assert.deepEqual([last.total, last.entries.map((e) => e.name)], [3, ['b.md']]);
    assert.deepEqual([kb.list('/lst', { offset: 10 }).total, kb.list('/lst', { offset: 10 }).entries], [3, []]);
    assert.throws(() => kb.list('/lst', { sort: 'colour' as never }), InvalidEdit);
  });

  test('entry describes any node; bulk moves or deletes several in one transaction', () => {
    kb.write('/bulk/a/x.md', 'x\n');
    kb.write('/bulk/a/y.md', 'y y\n');
    kb.write('/bulk/b.md', 'b\n');
    const root = kb.entry('/');
    assert.equal(root.kind, 'folder');
    assert.ok((root.files ?? 0) >= 3);
    assert.deepEqual([kb.entry('/bulk').files, kb.entry('/bulk').folders, kb.entry('/bulk/a/y.md').nwords], [3, 1, 2]);

    // A path inside a listed folder goes with the folder.
    assert.deepEqual(kb.bulk('move', ['/bulk/b.md', '/bulk/a/x.md', '/bulk/a'], { to: '/moved' }), {
      op: 'move',
      to: '/moved',
      done: ['/bulk/a', '/bulk/b.md'],
      skipped: ['/bulk/a/x.md'],
    });
    assert.deepEqual(kb.list('/moved').entries.map((e) => e.path), ['/moved/a', '/moved/b.md']);
    // All or nothing: the second path fails, so the first stays where it was.
    assert.throws(() => kb.bulk('move', ['/moved/b.md', '/nope.md'], { to: '/bulk' }), NotFound);
    assert.equal(kb.entry('/moved/b.md').kind, 'file');

    assert.deepEqual(kb.bulk('delete', ['/moved/a', '/moved/b.md']).done, ['/moved/a', '/moved/b.md']);
    assert.equal(kb.entry('/moved').files, 0);
  });
});
