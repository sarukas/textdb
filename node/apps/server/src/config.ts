import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** A store folder the server may sync with a directory on its own disk. */
export interface SyncLinkConfig {
  prefix: string;
  dir: string;
}

export interface Config {
  /** A SQLite file, or a `postgres://` URL: whatever `TEXTDB_STORE` (or `TEXTDB_DB`) names. */
  store: string;
  extension: string | undefined;
  /**
   * Answer nothing without a bearer (`TEXTDB_REQUIRE_TOKEN=1`).
   *
   * Off by default, where no bearer means the owner. On, every request carries a token and the
   * owner arrives with one too, of an `admin`-kind account -- which is the deployment the
   * extension's `admin` kind exists for.
   */
  requireToken: boolean;
  port: number;
  /** Interface to listen on. Loopback by default: the API has no authentication. */
  host: string;
  /** The built web UI, served when present. */
  webDist: string;
  /** Folders the web UI can sync, from `TEXTDB_SYNC`. */
  sync: SyncLinkConfig[];
  /** The textdb CLI that runs the syncs, from `TEXTDB_CLI`; found in the repository's build otherwise. */
  cli: string | undefined;
}

export function configFromEnv(env: NodeJS.ProcessEnv = process.env): Config {
  const host = env.HOST || '127.0.0.1';
  const loopback = ['127.0.0.1', 'localhost', '::1'].includes(host);
  return {
    // `TEXTDB_STORE` is what the CLI calls it; `TEXTDB_DB` is what this server always called it.
    store: env.TEXTDB_STORE || env.TEXTDB_DB || './kb.db',
    extension: env.TEXTDB_SQLITE_EXT || undefined,
    // Asked for, or implied by listening somewhere other than loopback: a server reachable from
    // the network must not answer as the owner to whoever knocks.
    requireToken: env.TEXTDB_REQUIRE_TOKEN === '1' || env.TEXTDB_REQUIRE_TOKEN === 'true' || !loopback,
    port: Number(env.PORT || 4317),
    host,
    webDist: fileURLToPath(new URL('../../web/dist', import.meta.url)),
    sync: parseSyncLinks(env.TEXTDB_SYNC),
    cli: env.TEXTDB_CLI || undefined,
  };
}

/**
 * `TEXTDB_SYNC`: `/folder=directory` pairs separated by `;` or new lines, e.g.
 * `/handbook=C:\src\handbook;/notes=/home/me/notes`. Only these directories can be synced from
 * the web UI: the API never takes a directory from a request.
 */
export function parseSyncLinks(value: string | undefined): SyncLinkConfig[] {
  if (!value) return [];
  return value
    .split(/[;\n]/)
    .map((pair) => pair.trim())
    .filter(Boolean)
    .map((pair) => {
      const eq = pair.indexOf('=');
      const prefix = pair.slice(0, eq).trim();
      const dir = pair.slice(eq + 1).trim();
      if (eq <= 0 || !prefix.startsWith('/') || !dir) {
        throw new Error(`TEXTDB_SYNC: expected /folder=directory, got "${pair}"`);
      }
      return { prefix: prefix.length > 1 ? prefix.replace(/\/+$/, '') : prefix, dir: path.resolve(dir) };
    });
}
