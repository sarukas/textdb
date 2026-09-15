import assert from 'node:assert/strict';
import { after, before, describe, test } from 'node:test';
import { type Corpus, InvalidEdit, openCorpus } from '../src/index.ts';
import { type TempDir, tempStore } from './helpers.ts';

/** A note with the front matter these tests ask about. */
function note(title: string, status: string, tags: string[], priority: number, extra = ''): string {
  return `---\ntitle: ${title}\nstatus: ${status}\ntags: [${tags.join(', ')}]\npriority: ${priority}\nproject:\n  name: atlas\n---\n${extra}\n# ${title}\n\nbody\n`;
}

describe('front-matter properties', () => {
  let tmp: TempDir;
  let kb: Corpus;

  before(() => {
    tmp = tempStore();
    kb = openCorpus({ db: tmp.db, author: 'tester' });
    kb.write('/a.md', note('A', 'draft', ['cvm', 'telco'], 5));
    kb.write('/b.md', note('B', 'review', ['telco'], 2));
    kb.write('/c.md', note('C', 'draft', ['cvm'], 1));
    kb.write('/plain.md', '# Plain\n\nno front matter\n');
  });

  after(() => {
    kb.close();
    tmp.remove();
  });

  test('lists the properties in use, counting documents rather than values', () => {
    const byKey = new Map(kb.propertyKeys().map((k) => [k.key, k]));
    // Three documents carry `tags`; five rows of them, because a list is a row per element.
    assert.equal(byKey.get('tags')?.docs, 3);
    assert.equal(byKey.get('status')?.valuesN, 2);
    // A UI offers `>` only where it means something.
    assert.equal(byKey.get('priority')?.kind, 'number');
    assert.equal(byKey.get('status')?.kind, 'text');
    assert.ok(byKey.has('project.name'), 'nested keys are dotted');
  });

  test('narrows names by prefix, which is what the autosuggest calls', () => {
    assert.deepEqual(
      kb.propertyKeys({ prefix: 'pro' }).map((k) => k.key),
      ['project.name'],
    );
  });

  test('lists a property’s values, most-used first', () => {
    assert.deepEqual(
      kb.propertyValues('status').map((v) => [v.value, v.docs]),
      [
        ['draft', 2],
        ['review', 1],
      ],
    );
    assert.deepEqual(
      kb.propertyValues('tags', { prefix: 'tel' }).map((v) => v.value),
      ['telco'],
    );
  });

  test('finds documents by one property and by several', () => {
    const paths = (q: string) => kb.propertyFind(q).map((h) => h.path);
    assert.deepEqual(paths('status:draft'), ['/a.md', '/c.md']);
    // A list containing a value is an ordinary equality, because lists are rows.
    assert.deepEqual(paths('tags:telco'), ['/a.md', '/b.md']);
    assert.deepEqual(paths('status:draft tags:telco'), ['/a.md'], 'a space means AND');
    assert.equal(paths('status:draft OR status:review').length, 3);
    assert.deepEqual(paths('-status:draft'), ['/b.md']);
    assert.deepEqual(paths('NOT status:draft'), ['/b.md']);
    assert.deepEqual(paths('project.name:atlas').length, 3);
    // Numbers compare numerically: lexically "5" would sort below "2".
    assert.deepEqual(paths('priority:>3'), ['/a.md']);
    assert.deepEqual(paths('tags:c*'), ['/a.md', '/c.md'], 'starts with');
    assert.deepEqual(paths('title:~B'), ['/b.md'], 'contains, ignoring case');
  });

  test('`!=` means has the property but not as that value', () => {
    const paths = (q: string) => kb.propertyFind(q).map((h) => h.path);
    // /plain.md has no status at all, so it is not a document whose status is not draft.
    assert.deepEqual(paths('status:!=draft'), ['/b.md']);
    assert.equal(paths('status:draft').length + paths('status:!=draft').length, 3);
  });

  test('a property search is over documents that have properties', () => {
    // An empty query is everything with front matter — not /plain.md, which has none.
    assert.equal(kb.propertyFind('').length, 3);
    assert.ok(!kb.propertyFind('').some((h) => h.path === '/plain.md'));
  });

  test('carries the whole front matter, so a table needs no query per cell', () => {
    const [hit] = kb.propertyFind('status:draft tags:telco');
    assert.equal(hit?.frontmatter?.status, 'draft');
    assert.deepEqual(hit?.frontmatter?.tags, ['cvm', 'telco']);
    assert.equal(hit?.frontmatter?.priority, 5);
    assert.ok(hit!.nbytes > 0);
  });

  test('follows edits: the index moves the document between answers', () => {
    const paths = (q: string) => kb.propertyFind(q).map((h) => h.path);
    kb.write('/c.md', note('C', 'published', ['cvm'], 1));
    assert.deepEqual(paths('status:draft'), ['/a.md']);
    assert.deepEqual(paths('status:published'), ['/c.md']);
    kb.remove('/b.md');
    assert.deepEqual(paths('tags:telco'), ['/a.md']);
  });

  test('a malformed query raises, naming where it went wrong', () => {
    assert.throws(() => kb.propertyFind('status:draft AND'), (e: unknown) => {
      assert.ok(e instanceof InvalidEdit, `got ${String(e)}`);
      assert.match(String(e), /ends early/);
      return true;
    });
  });
});
