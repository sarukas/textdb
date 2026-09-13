import assert from 'node:assert/strict';
import { DatabaseSync } from 'node:sqlite';
import { after, before, test } from 'node:test';
import { type Change, type Corpus, openCorpus } from '../src/index.ts';
import { type TempDir, tempStore, waitFor } from './helpers.ts';

let tmp: TempDir;
let kb: Corpus;

before(() => {
  tmp = tempStore();
  kb = openCorpus({ db: tmp.db });
});

after(() => {
  kb.close();
  tmp.remove();
});

test('the watcher sees commits from another connection and from the main connection', async () => {
  kb.write('/before.md', 'ignored\n');
  const seen: Change[] = [];
  const watcher = kb.watch({ onChange: (change) => seen.push(change), intervalMs: 20 });
  try {
    const other = new DatabaseSync(tmp.db, { allowExtension: true });
    other.loadExtension(kb.extension);
    other.exec('PRAGMA busy_timeout=30000');
    other.prepare("SELECT textdb_write('/other.md', 'from the cli\n', NULL, 'cli')").get();
    other.close();

    await waitFor(() => seen.find((c) => c.path === '/other.md'));
    kb.write('/other.md', 'from the server\n', { author: 'server' });
    await waitFor(() => seen.find((c) => c.op === 'commit'));

    assert.deepEqual(
      seen.map((c) => [c.op, c.path, c.version, c.author]),
      [
        ['create', '/other.md', 1, 'cli'],
        ['commit', '/other.md', 2, 'server'],
      ],
    );
    assert.equal(watcher.lastSeq, kb.lastSeq());
  } finally {
    watcher.close();
  }
});

test('the watcher replays from since', async () => {
  const since = kb.lastSeq();
  kb.write('/replay/a.md', 'a\n');
  kb.write('/replay/a.md', 'b\n');
  const seen: Change[] = [];
  const watcher = kb.watch({ since, onChange: (change) => seen.push(change), intervalMs: 20, batchSize: 2 });
  try {
    await waitFor(() => (seen.length === 3 ? true : undefined));
    assert.deepEqual(seen.map((c) => c.op), ['mkdir', 'create', 'commit']);
  } finally {
    watcher.close();
  }
});
