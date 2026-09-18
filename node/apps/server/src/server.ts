import type { AddressInfo } from 'node:net';
import { type ServerType, serve } from '@hono/node-server';
import type { CorpusApi } from '@textdb/node';
import { createApp } from './app.ts';
import { AssetService } from './assets.ts';
import type { SyncLinkConfig } from './config.ts';
import { Corpora } from './corpora.ts';
import { ChangeHub } from './hub.ts';
import { findCli, SyncService } from './sync.ts';

export interface ServerOptions {
  /** Folders the web UI can sync with directories on this machine. */
  sync?: SyncLinkConfig[];
  /** The textdb CLI that runs syncs. */
  cli?: string | undefined;
  /** A SQLite file, or a `postgres://` URL. */
  store: string;
  extension?: string | undefined;
  /** Answer nothing without a bearer; see `AppOptions`. */
  requireToken?: boolean;
  /** 0 picks a free port. */
  port: number;
  /** Defaults to loopback: the API has no authentication. */
  host?: string;
  webDist: string;
  pingMs?: number;
  watchIntervalMs?: number;
}

export interface RunningServer {
  url: string;
  /** The owner's corpus, which is what the change feed and the sync services read. */
  corpus: CorpusApi;
  corpora: Corpora;
  hub: ChangeHub;
  close(): Promise<void>;
}

export async function startServer(options: ServerOptions): Promise<RunningServer> {
  const corpora = new Corpora({ store: options.store, extension: options.extension });
  // The owner's corpus serves the change feed and the sync services: both are the server's own
  // work rather than any one request's, and neither is answered in an account's view.
  const corpus = await corpora.owner();
  const hub = new ChangeHub(corpus, { intervalMs: options.watchIntervalMs });
  const sync = options.sync?.length ? new SyncService(corpus, options.sync, findCli(options.cli)) : null;
  const assets = sync ? new AssetService(sync, options.store) : null;
  const app = createApp(corpora, hub, {
    webDist: options.webDist,
    pingMs: options.pingMs ?? 15_000,
    sync,
    assets,
    host: options.host ?? '127.0.0.1',
    requireToken: options.requireToken,
  });

  const host = options.host ?? '127.0.0.1';
  let server: ServerType;
  try {
    server = await new Promise<ServerType>((resolve, reject) => {
      const s = serve({ fetch: app.fetch, port: options.port, hostname: host }, () => resolve(s));
      s.once('error', reject);
    });
  } catch (error) {
    hub.close();
    await corpora.close();
    throw error;
  }

  const { port } = server.address() as AddressInfo;
  return {
    url: `http://${host.includes(':') ? `[${host}]` : host}:${port}`,
    corpus,
    corpora,
    hub,
    close: async () => {
      hub.close();
      await new Promise<void>((resolve) => {
        server.close(() => resolve());
        if ('closeAllConnections' in server) server.closeAllConnections();
      });
      await corpora.close();
    },
  };
}
