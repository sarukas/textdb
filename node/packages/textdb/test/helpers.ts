import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

export interface TempDir {
  dir: string;
  db: string;
  remove(): void;
}

export function tempStore(): TempDir {
  const dir = mkdtempSync(path.join(tmpdir(), 'textdb-node-'));
  return {
    dir,
    db: path.join(dir, 'kb.db'),
    // Windows keeps the WAL files locked for a moment after close.
    remove: () => rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 }),
  };
}

export async function waitFor<T>(probe: () => T | undefined, timeoutMs = 5000): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = probe();
    if (value !== undefined) return value;
    if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs} ms`);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}
