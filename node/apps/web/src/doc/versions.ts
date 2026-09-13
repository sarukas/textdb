import { api } from "../api";

const MAX = 200;
const cache = new Map<string, Promise<string>>();

/** Text of a file at a version. Versions never change, so they are cached for the session. */
export function getVersionText(path: string, version: number): Promise<string> {
  if (version <= 0) return Promise.resolve("");
  const key = `${path}@${version}`;
  let p = cache.get(key);
  if (!p) {
    p = api.file(path, version).then((f) => f.content);
    p.catch(() => cache.delete(key));
    cache.set(key, p);
    if (cache.size > MAX) {
      const oldest = cache.keys().next().value;
      if (oldest !== undefined) cache.delete(oldest);
    }
  }
  return p;
}
