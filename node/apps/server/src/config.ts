import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** A store folder the server may sync with a directory on its own disk. */
export interface SyncLinkConfig {
  prefix: string;
  dir: string;
}

export interface Config {
  db: string;
  extension: string | undefined;
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
  return {
    db: env.TEXTDB_DB || './kb.db',
    extension: env.TEXTDB_SQLITE_EXT || undefined,
    port: Number(env.PORT || 4317),
    host: env.HOST || '127.0.0.1',
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
