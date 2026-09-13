import { readFile, stat } from 'node:fs/promises';
import path from 'node:path';
import type { Context } from 'hono';

const CONTENT_TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.txt': 'text/plain; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.wasm': 'application/wasm',
};

async function isFile(file: string): Promise<boolean> {
  return stat(file).then((s) => s.isFile(), () => false);
}

/**
 * Serves the built web UI from `distDir` when it exists. Paths without an extension fall back
 * to index.html so client-side routes survive a reload; a missing asset stays a 404.
 */
export function webApp(distDir: string) {
  const root = path.resolve(distDir);
  const index = path.join(root, 'index.html');
  return async (c: Context): Promise<Response> => {
    if (!(await isFile(index))) return c.notFound();
    const requested = path.resolve(root, `.${c.req.path}`);
    let file: string;
    if (requested.startsWith(root + path.sep) && (await isFile(requested))) file = requested;
    else if (path.extname(c.req.path) === '') file = index;
    else return c.notFound();

    const headers: Record<string, string> = {
      'Content-Type': CONTENT_TYPES[path.extname(file).toLowerCase()] ?? 'application/octet-stream',
    };
    if (file === index) headers['Cache-Control'] = 'no-cache';
    return c.body(new Uint8Array(await readFile(file)), 200, headers);
  };
}
