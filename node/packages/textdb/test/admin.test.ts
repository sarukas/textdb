import assert from 'node:assert/strict';
import { after, before, describe, test } from 'node:test';
import { Access, AssetStores, Cli, Forbidden, NotFound, findCli, openCorpus } from '../src/index.ts';
import { tempStore } from './helpers.ts';

/**
 * Configuration through the CLI: accounts, tokens, shares, asset stores.
 *
 * These are the store's rules, not this SDK's, so what is worth asserting is that the answers come
 * back as data and the refusals as the error classes their codes mean -- a `Forbidden` for a
 * command an account may not run, a `NotFound` for a store that is not declared. Without a CLI
 * build there is nothing to run them with, so the suite says so and skips rather than passing
 * quietly.
 */
const cli = findCli(process.env.TEXTDB_CLI);

describe('access and asset stores', { skip: cli ? false : 'the textdb CLI is not built (or name it with TEXTDB_CLI)' }, () => {
  let tmp: ReturnType<typeof tempStore>;
  let access: Access;
  let stores: AssetStores;

  before(() => {
    tmp = tempStore();
    const corpus = openCorpus({ db: tmp.db });
    corpus.write('/legal/nda.md', 'secret\n');
    corpus.write('/notes/open.md', 'open\n');
    corpus.close();
    const runner = new Cli({ store: tmp.db, cli });
    access = new Access(runner);
    stores = new AssetStores(runner);
  });

  after(() => tmp.remove());

  test('an account, a share and a token, and the refusals an account meets', async () => {
    const made = await access.createAccount('reader', { kind: 'agent' });
    assert.deepEqual([made.account, made.kind], ['reader', 'agent']);
    assert.deepEqual(
      (await access.accounts()).map((a) => [a.name, a.kind, a.disabled, a.shares]),
      [['reader', 'agent', false, 0]],
    );

    const share = await access.grant('reader', '/notes', 'ro', { alias: 'notes' });
    assert.deepEqual([share.account, share.alias, share.rights], ['reader', 'notes', 'ro']);
    // Of a path: who can reach it, which is the owner's other way of asking the same question.
    assert.deepEqual(
      (await access.shares({ path: '/notes' })).map((s) => s.account),
      ['reader'],
    );

    // The bearer exists in this answer and nowhere else.
    const { bearer } = await access.createToken('reader', { label: 'a laptop' });
    assert.match(bearer, /^tdb_/);
    const [token] = await access.tokens('reader');
    assert.deepEqual([token?.account, token?.label, token?.live], ['reader', 'a laptop', true]);
    assert.ok(!JSON.stringify(token).includes(bearer), 'the token row carried the bearer itself');

    // As that account: the store refuses every one of these, and a refusal is a Forbidden and not
    // a failure of this SDK's own.
    const mine = access.as({ token: bearer });
    await assert.rejects(() => mine.accounts(), (e: unknown) => e instanceof Forbidden && e.code === 'TX005');
    await assert.rejects(
      () => mine.grant('reader', '/legal', 'rw'),
      (e: unknown) => e instanceof Forbidden,
    );
    // And nothing of it happened.
    assert.deepEqual(
      (await access.shares({ account: 'reader' })).map((s) => s.alias),
      ['notes'],
    );

    // Disabled and enabled again, with its shares kept either way.
    await access.setAccountEnabled('reader', false);
    assert.equal((await access.accounts())[0]?.disabled, true);
    await access.setAccountEnabled('reader', true);
    assert.equal((await access.accounts())[0]?.disabled, false);

    assert.deepEqual(await access.revokeToken(token!.id), { revoked: 1 });
    const revoked = (await access.tokens('reader'))[0];
    assert.equal(revoked?.live, false);
    assert.ok(revoked?.revoked_at, 'a revoked token says when it was revoked');
  });

  test('a name that starts with a dash is a name, not a flag', async () => {
    // Every value a caller supplies is data. Passed bare, `-weird` is a flag to the CLI's parser,
    // which exits with a usage message and no JSON -- a 500 out of a server, for what is an
    // ordinary request. The `--` and `--flag=value` forms are what keep that from happening.
    const made = await access.createAccount('-weird', { kind: 'agent' });
    assert.equal(made.account, '-weird');
    const share = await access.grant('-weird', '/notes', 'ro', { alias: '-alias' });
    assert.deepEqual([share.account, share.alias], ['-weird', '-alias']);
    assert.deepEqual(
      (await access.shares({ account: '-weird' })).map((s) => s.alias),
      ['-alias'],
    );
    const minted = await access.createToken('-weird', { label: '-x' });
    assert.match(minted.bearer, /^tdb_/);
    assert.equal((await access.tokens('-weird'))[0]?.label, '-x');
    await access.renameShare('-weird', '-alias', '-other');
    await access.revokeShare('-weird', '-other');
    await access.setAccountEnabled('-weird', false);
    assert.equal((await access.accounts()).find((a) => a.name === '-weird')?.disabled, true);
  });

  test('a single-root account is converted to aliased shares', async () => {
    // `--multi` is required by the command and was the bug this suite exists to have caught: the
    // conversion is one way and changes every path the account sees, so it is spelled out.
    await access.createAccount('one', { kind: 'agent', root: '/legal' });
    assert.equal((await access.accounts()).find((a) => a.name === 'one')?.root, '/legal');
    await access.convertAccount('one', { alias: 'legal' });
    assert.deepEqual(
      (await access.shares({ account: 'one' })).map((s) => [s.alias, s.path]),
      [['legal', '/legal']],
    );
  });

  test('a store is declared for everyone and bound for this computer', async () => {
    assert.deepEqual(await stores.list(), []);

    const bucket = `${tmp.dir}/bucket`;
    const declared = await stores.put('team', bucket);
    assert.deepEqual(
      declared.map((s) => [s.name, s.driver, s.root, s.bound_to]),
      [['team', 'local', bucket, null]],
    );

    // A name carrying `=` would bind a different store, since `--bind` splits at the first one.
    await assert.rejects(() => stores.bind('team=evil', 'z'), { code: 'TX004' });
    // And a store that was never declared is not one this computer can be bound to.
    await assert.rejects(() => stores.remove('teem'), (e: unknown) => e instanceof NotFound);

    assert.deepEqual(
      (await stores.remove('team')).map((s) => s.name),
      [],
    );
  });
});
