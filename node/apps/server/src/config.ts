import { fileURLToPath } from 'node:url';

export interface Config {
  db: string;
  extension: string | undefined;
  port: number;
  /** Interface to listen on. Loopback by default: the API has no authentication. */
  host: string;
  /** The built web UI, served when present. */
  webDist: string;
}

export function configFromEnv(env: NodeJS.ProcessEnv = process.env): Config {
  return {
    db: env.TEXTDB_DB || './kb.db',
    extension: env.TEXTDB_SQLITE_EXT || undefined,
    port: Number(env.PORT || 4317),
    host: env.HOST || '127.0.0.1',
    webDist: fileURLToPath(new URL('../../web/dist', import.meta.url)),
  };
}
