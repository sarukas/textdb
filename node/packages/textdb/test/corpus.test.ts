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
});
