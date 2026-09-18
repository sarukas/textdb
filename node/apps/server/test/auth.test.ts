import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { after, before, describe, test } from 'node:test';
import { type RunningServer, startServer } from '../src/server.ts';
import { findCli } from '../src/sync.ts';
import { tempDir } from './helpers.ts';

/**
 * The server serves an account.
 *
 * K7 of the #12 catalogue -- "web app login with its token" -- was answered at the CLI and JSON
 * layer, because the HTTP server had no notion of a bearer at all: every request was the owner's,
 * and the documentation said so out loud ("the API has no authentication"). This is its HTTP half.
 *
 * The account and its token are made with the CLI, which is where those commands live; without a
 * build of it there is nothing to make them with, so the suite says so and stops rather than
 * passing quietly.
 */
describe('a bearer per request', () => {
  const cli = findCli(undefined);
  let tmp: ReturnType<typeof tempDir>;
  let server: RunningServer;
  let bearer = '';

  const call = async (url: string, token?: string) => {
    const res = await fetch(`${server.url}${url}`, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
    });
    const type = res.headers.get('content-type') ?? '';
    return { status: res.status, body: type.includes('json') ? await res.json() : await res.text() };
  };

  before(async () => {
    tmp = tempDir();
    const store = path.join(tmp.dir, 'kb.db');
    if (cli) {
      const textdb = (args: string[], stdin?: string) =>
        execFileSync(cli, ['--store', store, ...args], { encoding: 'utf8', input: stdin ?? '' });
      textdb(['init']);
      textdb(['write', '/legal/nda.md'], 'secret\n');
      textdb(['write', '/notes/open.md'], 'open\n');
      textdb(['account', 'create', 'reader']);
      textdb(['access', 'grant', 'reader', '/notes', 'ro', '--as', 'notes']);
      bearer = (JSON.parse(textdb(['--json', 'token', 'create', 'reader'])) as { bearer: string }).bearer;
    }
    server = await startServer({ store, port: 0, webDist: path.join(tmp.dir, 'web'), pingMs: 100, watchIntervalMs: 20 });
  });

  after(async () => {
    await server.close();
    tmp.remove();
  });

  test('no bearer is the owner, and an unusable one is refused', async () => {
    const who = await call('/api/whoami');
    assert.deepEqual(who.body, { account: null, admin: true, kind: 'owner', namespace: 'store', shares: [] });

    // Not 404 and not 500: the store refuses the bearer, and a refusal is a 403.
    const bad = await call('/api/whoami', 'tdb_nothing-of-the-kind');
    assert.equal(bad.status, 403);
    assert.equal(bad.body.code, 'TX005');
  });

  test('a bearer is answered in that account’s view, and nothing outside it', async (t) => {
    if (!cli) return t.skip('no textdb CLI build to create an account with');

    const who = await call('/api/whoami', bearer);
    assert.deepEqual(who.body, {
      account: 'reader',
      admin: false,
      kind: 'agent',
      namespace: 'aliased',
      shares: [{ alias: 'notes', rights: 'ro', node_id: who.body.shares[0].node_id, dormant: false }],
    });

    // Its root is its shares, under their aliases, with the rights it holds them by.
    const root = await call('/api/ls?path=/', bearer);
    assert.deepEqual(
      root.body.map((e: { path: string; rights: string }) => [e.path, e.rights]),
      [['/notes', 'ro']],
    );

    // Its own file, at its own path, which is not where the store keeps it.
    const own = await call('/api/file?path=/notes/open.md', bearer);
    assert.equal(own.status, 200);
    assert.equal(own.body.path, '/notes/open.md');

    // A folder it was never granted is not there -- 404, not 403: the difference is the point.
    const hidden = await call('/api/file?path=/legal/nda.md', bearer);
    assert.equal(hidden.status, 404);
    assert.equal(hidden.body.code, 'TX003');

    // And the store's own paths are not its paths: the owner's spelling finds nothing either.
    assert.equal((await call('/api/ls?path=/legal', bearer)).status, 404);

    // The owner still sees everything, on the same server, at the same time.
    const all = await call('/api/ls?path=/');
    assert.deepEqual(
      all.body.map((e: { path: string }) => e.path),
      ['/legal', '/notes'],
    );
  });

  test('a read-only share is refused a write, and says so as forbidden', async (t) => {
    if (!cli) return t.skip('no textdb CLI build to create an account with');
    const res = await fetch(`${server.url}/api/file`, {
      method: 'PUT',
      headers: { Authorization: `Bearer ${bearer}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ path: '/notes/open.md', content: 'mine now\n' }),
    });
    assert.equal(res.status, 403);
    assert.equal(((await res.json()) as { code: string }).code, 'TX005');
  });

  test('a browser session keeps its bearer in a cookie the page cannot read', async (t) => {
    if (!cli) return t.skip('no textdb CLI build to create an account with');

    // Logging in checks the token before handing out a session, and answers who it is.
    const login = await fetch(`${server.url}/api/session`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token: bearer }),
    });
    assert.equal(login.status, 200);
    assert.equal(((await login.json()) as { account: string }).account, 'reader');
    const cookie = login.headers.get('set-cookie') ?? '';
    assert.match(cookie, /^textdb_session=/);
    // A credential a script can read is a credential another site's script can read.
    assert.match(cookie, /HttpOnly/);
    assert.match(cookie, /SameSite=Strict/);

    // The cookie alone is enough, which is what an EventSource and an <img src> have.
    const jar = cookie.split(';')[0] ?? '';
    const who = await fetch(`${server.url}/api/whoami`, { headers: { cookie: jar } });
    assert.equal(((await who.json()) as { account: string }).account, 'reader');

    // A token that does not work gets no session at all.
    const refused = await fetch(`${server.url}/api/session`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token: 'tdb_nothing-of-the-kind' }),
    });
    assert.equal(refused.status, 403);
    assert.equal(refused.headers.get('set-cookie'), null);

    // And logging out clears it, whatever the browser still holds.
    const out = await fetch(`${server.url}/api/session`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', cookie: jar },
      body: JSON.stringify({ token: null }),
    });
    assert.match(out.headers.get('set-cookie') ?? '', /Max-Age=0/);
  });

  test('one corpus per account, and idle ones are closed', async (t) => {
    if (!cli) return t.skip('no textdb CLI build to create an account with');
    // A bearer belongs to a connection, so the pool holds one corpus for this account -- not one
    // per request, and never a shared connection re-authenticated between them.
    await call('/api/whoami', bearer);
    await call('/api/whoami', bearer);
    assert.equal(server.corpora.open, 1);
  });
});

describe('a server that answers nothing without a bearer', () => {
  let tmp: ReturnType<typeof tempDir>;
  let server: RunningServer;

  before(async () => {
    tmp = tempDir();
    server = await startServer({
      store: path.join(tmp.dir, 'kb.db'),
      port: 0,
      webDist: path.join(tmp.dir, 'web'),
      requireToken: true,
    });
  });

  after(async () => {
    await server.close();
    tmp.remove();
  });

  test('401 without one, because "say who you are" is not "you may not"', async () => {
    const res = await fetch(`${server.url}/api/info`);
    assert.equal(res.status, 401);
    const body = (await res.json()) as { message: string };
    assert.match(body.message, /Authorization: Bearer/);
  });
});
