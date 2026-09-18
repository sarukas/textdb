import assert from 'node:assert/strict';
import { after, before, describe, test } from 'node:test';
import { type Corpus, openCorpus } from '../src/index.ts';
import { type TempDir, tempStore } from './helpers.ts';

describe('markdown outlines', () => {
  let tmp: TempDir;
  let kb: Corpus;

  before(() => {
    tmp = tempStore();
    kb = openCorpus({ db: tmp.db, author: 'tester' });
    kb.write(
      '/notes/a.md',
      '---\ntitle: A\n---\n# Alpha\nintro words here\n\n## Goals\nwe want things\n\n### Detail\nfine print\n\n## Next Steps\nship it\n',
    );
    kb.write('/notes/b.md', '# Beta\nbody\n\n## next steps\nlater\n');
    kb.write('/other/c.md', '# Gamma\nonly words\n');
  });

  after(() => {
    kb.close();
    tmp.remove();
  });

  test('one document reads as its table of contents', () => {
    const rows = kb.outline('/notes/a.md');
    assert.deepEqual(
      rows.map((r) => [r.heading, r.level]),
      [
        ['Alpha', 1],
        ['Goals', 2],
        ['Detail', 3],
        ['Next Steps', 2],
      ],
    );
    // The breadcrumb is the nesting; the heading is the last component of it.
    const detail = rows[2];
    assert.ok(detail);
    assert.equal(detail.headingPath, 'Alpha / Goals / Detail');
    assert.equal(detail.heading, 'Detail');
  });

  test('a folder takes everything below it and nothing beside it', () => {
    assert.deepEqual(
      kb.outline('/other').map((r) => r.path),
      ['/other/c.md'],
    );
    assert.equal(kb.outline('/').length, 7);
  });

  test('headings match folded, by whole, prefix or substring', () => {
    const paths = (heading: string, match: 'exact' | 'prefix' | 'contains') =>
      kb.outline('/', { heading, match }).map((r) => r.path);
    assert.deepEqual(paths('NEXT STEPS', 'exact'), ['/notes/a.md', '/notes/b.md']);
    assert.deepEqual(paths('next', 'prefix'), ['/notes/a.md', '/notes/b.md']);
    assert.deepEqual(paths('tep', 'contains'), ['/notes/a.md', '/notes/b.md']);
    assert.deepEqual(paths('nothing-here', 'prefix'), []);
  });

  test('level caps the depth', () => {
    assert.deepEqual(
      kb.outline('/', { level: 1 }).map((r) => r.heading),
      ['Alpha', 'Beta', 'Gamma'],
    );
  });

  test('word counts compose and every row carries its file', () => {
    const [root, , detail] = kb.outline('/notes/a.md');
    assert.ok(root && detail);
    const nested = kb
      .outline('/notes/a.md')
      .slice(1)
      .reduce((n, r) => n + (r.nwords ?? 0), 0);
    assert.equal(root.nwordsTotal, (root.nwords ?? 0) + nested);
    // A leaf's own count and its total are the same number.
    assert.equal(detail.nwords, detail.nwordsTotal);
    // The file columns save a second query per row.
    assert.equal(root.nbytes, kb.stat('/notes/a.md').nbytes);
    assert.equal(root.version, 1);
    assert.equal(root.updatedBy, 'tester');
  });

  test('headingNames folds the two spellings into one suggestion', () => {
    const [next, ...rest] = kb.headingNames('/', { starts: 'ne' });
    assert.ok(next);
    assert.equal(rest.length, 0);
    assert.equal(next.sections, 2);
    assert.equal(next.docs, 2);
  });
});
