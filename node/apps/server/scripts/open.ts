import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { findCli, runCli } from '@textdb/node';
import { startServer } from '../src/server.ts';

/**
 * Open the web app on the directory you synced.
 *
 * `textdb sync` pairs a directory with a folder of a store and records it in `.textdb/config`, so
 * everything the server needs is already on disk: which store, which folder, which directory. This
 * asks the CLI for that pairing -- `textdb config --json`, which reports it for the directory it is
 * run in -- and starts the server on it, rather than making someone spell the same three things out
 * as environment variables.
 *
 *   npm run open -- /path/to/vault          # the directory textdb sync was run on
 *   npm run open -- . --no-browser          # from inside it, without opening a browser
 *   PORT=4318 npm run open -- /path/to/vault
 */
interface Pairing {
  dir: string;
  prefix: string;
  store: string;
  account: string | null;
}

async function pairingOf(cli: string, dir: string): Promise<Pairing> {
  const { stdout, stderr, status } = await runCli(cli, ['--json', 'config'], { cwd: dir });
  let answered: { directory?: Pairing | null } = {};
  try {
    answered = JSON.parse(stdout) as typeof answered;
  } catch {
    throw new Error(`textdb config did not answer JSON in ${dir}: ${(stderr || stdout).trim().slice(0, 300)}`);
  }
  if (status !== 0) throw new Error((stderr || stdout).trim().slice(0, 300));
  if (!answered.directory) {
    throw new Error(
      `${dir} is not a synced directory: nothing in it or above it is paired with a folder of a store.\n` +
        `Sync it first — textdb sync FOLDER ${dir} — or start the server with TEXTDB_DB and TEXTDB_SYNC yourself.`,
    );
  }
  return answered.directory;
}

/** Show the page in whatever this computer opens a URL with; never the reason the command fails. */
function browse(url: string): void {
  const [command, args] =
    process.platform === 'win32'
      ? ['cmd', ['/c', 'start', '', url]]
      : process.platform === 'darwin'
        ? ['open', [url]]
        : ['xdg-open', [url]];
  try {
    spawn(command, args, { detached: true, stdio: 'ignore', windowsHide: true }).unref();
  } catch {
    // The URL is on screen either way.
  }
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const wanted = args.filter((a) => !a.startsWith('-'));
  const noBrowser = args.includes('--no-browser');
  const where = path.resolve(wanted[0] ?? process.cwd());
  if (!existsSync(where)) throw new Error(`${where} is not there`);

  const cli = findCli(process.env.TEXTDB_CLI);
  if (!cli) {
    throw new Error('the textdb CLI was not found: build it (cargo build --release -p textdb-cli) or name it with TEXTDB_CLI');
  }
  const paired = await pairingOf(cli, where);
  const server = await startServer({
    store: paired.store,
    port: Number(process.env.PORT || 4317),
    host: process.env.HOST || '127.0.0.1',
    webDist: fileURLToPath(new URL('../../web/dist', import.meta.url)),
    sync: [{ prefix: paired.prefix, dir: paired.dir }],
    cli,
  });
  // The folder's own page, not the store's root: the directory is what was asked about.
  const url = `${server.url}/#${encodeURI(paired.prefix)}${paired.prefix.endsWith('/') ? '' : '/'}`;
  console.log(`${paired.dir}\n  ${paired.prefix} of ${paired.store}${paired.account ? ` (synced as ${paired.account})` : ''}\n  ${url}`);
  if (!noBrowser) browse(url);

  const stop = () => {
    void server.close().then(() => process.exit(0));
  };
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
