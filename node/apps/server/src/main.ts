import { configFromEnv } from './config.ts';
import { startServer } from './server.ts';

const config = configFromEnv();
const server = await startServer(config);
console.log(`textdb server listening on ${server.url} (store ${server.corpus.db})`);

for (const signal of ['SIGINT', 'SIGTERM'] as const) {
  process.once(signal, () => {
    void server.close().then(() => process.exit(0));
  });
}
