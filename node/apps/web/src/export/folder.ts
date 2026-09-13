/**
 * Exporting into a folder the user picks (File System Access API: Chrome and Edge). A plan comes
 * first: what on disk is new, changed or already identical, and which names cannot be written
 * here. Then only new and changed files are written. Nothing on disk is deleted and a file whose
 * bytes already match is not touched, so a git checkout shows exactly the changes made in textdb.
 */
import { checkNames, foldName, type NameProblem, type Platform } from "./names";

export interface ExportFile {
  /** `/`-separated, below the exported folder. */
  rel: string;
  nbytes: number;
}

export interface WritableFile {
  write(data: Uint8Array): Promise<void>;
  close(): Promise<void>;
  abort?(reason?: unknown): Promise<void>;
}

export interface FileHandleLike {
  kind: "file";
  name: string;
  getFile(): Promise<Blob>;
  createWritable(): Promise<WritableFile>;
}

/** The parts of `FileSystemDirectoryHandle` an export uses. */
export interface DirHandle {
  kind: "directory";
  name: string;
  values(): AsyncIterable<DirHandle | FileHandleLike>;
  getDirectoryHandle(name: string, options?: { create?: boolean }): Promise<DirHandle>;
  getFileHandle(name: string, options?: { create?: boolean }): Promise<FileHandleLike>;
}

export type Change = "new" | "changed" | "unchanged";

export interface PlannedFile extends ExportFile {
  change: Change;
}

export interface ExportPlan {
  files: PlannedFile[];
  /** Name problems and conflicts with what is on disk, blocking ones first. */
  problems: NameProblem[];
  counts: Record<Change, number>;
  bytesToWrite: number;
}

export interface PlanProgress {
  phase: "listing" | "comparing";
  done: number;
  total: number;
}

export interface PlanOptions {
  root: DirHandle;
  files: readonly ExportFile[];
  platform: Platform;
  /** SHA-256 (hex) of the stored content of `rels`: asked only for files whose size on disk matches. */
  storeHashes: (rels: string[]) => Promise<Map<string, string>>;
  diskHash?: (blob: Blob) => Promise<string>;
  onProgress?: (p: PlanProgress) => void;
  signal?: AbortSignal;
}

const HASH_BATCH = 500;
const WORKERS = 8;

const parentOf = (rel: string) => rel.slice(0, Math.max(0, rel.lastIndexOf("/")));
const nameOf = (rel: string) => rel.slice(rel.lastIndexOf("/") + 1);

export async function sha256Hex(blob: Blob): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", await blob.arrayBuffer());
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Run `fn` over `items`, `n` at a time. */
export async function pool<T>(items: readonly T[], n: number, fn: (item: T) => Promise<void>, signal?: AbortSignal): Promise<void> {
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      signal?.throwIfAborted();
      await fn(items[next++]!);
    }
  };
  await Promise.all(Array.from({ length: Math.min(n, items.length) }, worker));
}

interface Listing {
  byName: Map<string, DirHandle | FileHandleLike>;
  /** Folded name → the name on disk. */
  byFold: Map<string, string>;
}

export async function planFolderExport(o: PlanOptions): Promise<ExportPlan> {
  const insensitive = o.platform !== "linux";
  const conflicts: NameProblem[] = [];
  const conflict = (path: string, kind: "disk-case" | "disk-kind", detail: string) =>
    conflicts.push({ path, kind, detail, platforms: kind === "disk-case" ? [o.platform] : ["windows", "macos", "linux"], blocking: true });

  // One listing per folder the export touches, never more: a checkout's .git is not walked.
  const listings = new Map<string, Promise<Listing | null>>();
  let listed = 0;
  const list = (dir: string): Promise<Listing | null> => {
    let pending = listings.get(dir);
    if (!pending) {
      pending = (async () => {
        let handle = o.root;
        if (dir !== "") {
          const parent = await list(parentOf(dir));
          if (!parent) return null;
          const seg = nameOf(dir);
          const found = parent.byName.get(seg);
          if (!found) {
            const other = insensitive ? parent.byFold.get(foldName(seg, o.platform)) : undefined;
            if (other !== undefined) conflict(`${dir}/`, "disk-case", `exists on disk as “${other}”`);
            return null;
          }
          if (found.kind !== "directory") {
            conflict(`${dir}/`, "disk-kind", "is a file on disk; the export needs a folder here");
            return null;
          }
          handle = found;
        }
        const listing: Listing = { byName: new Map(), byFold: new Map() };
        for await (const entry of handle.values()) {
          listing.byName.set(entry.name, entry);
          listing.byFold.set(foldName(entry.name, o.platform), entry.name);
        }
        o.onProgress?.({ phase: "listing", done: ++listed, total: listings.size });
        return listing;
      })();
      listings.set(dir, pending);
    }
    return pending;
  };

  const change = new Map<string, Change>();
  const candidates: { rel: string; blob: Blob }[] = [];
  await pool(
    o.files,
    WORKERS,
    async (f) => {
      const listing = await list(parentOf(f.rel));
      const name = nameOf(f.rel);
      const found = listing?.byName.get(name);
      if (!found) {
        const other = listing && insensitive ? listing.byFold.get(foldName(name, o.platform)) : undefined;
        if (other !== undefined) conflict(f.rel, "disk-case", `exists on disk as “${other}”`);
        change.set(f.rel, "new");
      } else if (found.kind === "directory") {
        conflict(f.rel, "disk-kind", "is a folder on disk; the export needs a file here");
        change.set(f.rel, "new");
      } else {
        const blob = await found.getFile();
        if (blob.size !== f.nbytes) change.set(f.rel, "changed");
        else candidates.push({ rel: f.rel, blob });
      }
    },
    o.signal,
  );

  // Same size: compare content hashes, the store's computed on the server.
  const diskHash = o.diskHash ?? sha256Hex;
  let compared = 0;
  for (let i = 0; i < candidates.length; i += HASH_BATCH) {
    o.signal?.throwIfAborted();
    const batch = candidates.slice(i, i + HASH_BATCH);
    const stored = await o.storeHashes(batch.map((c) => c.rel));
    await pool(
      batch,
      WORKERS,
      async (c) => {
        change.set(c.rel, (await diskHash(c.blob)) === stored.get(c.rel) ? "unchanged" : "changed");
        o.onProgress?.({ phase: "comparing", done: ++compared, total: candidates.length });
      },
      o.signal,
    );
  }

  const files = o.files.map((f) => ({ ...f, change: change.get(f.rel) ?? "new" }));
  const counts: Record<Change, number> = { new: 0, changed: 0, unchanged: 0 };
  let bytesToWrite = 0;
  for (const f of files) {
    counts[f.change] += 1;
    if (f.change !== "unchanged") bytesToWrite += f.nbytes;
  }
  const problems = [...conflicts, ...checkNames(o.files.map((f) => f.rel), o.platform)].sort(
    (a, b) => Number(b.blocking) - Number(a.blocking) || a.path.localeCompare(b.path),
  );
  return { files, problems, counts, bytesToWrite };
}

export interface WriteFailure {
  rel: string;
  reason: string;
}

export interface WriteProgress {
  done: number;
  total: number;
  bytes: number;
  failures: WriteFailure[];
}

export interface WriteOptions {
  root: DirHandle;
  /** The new and changed files. */
  files: readonly ExportFile[];
  fetchBytes: (rel: string) => Promise<Uint8Array>;
  onProgress?: (p: WriteProgress) => void;
  shouldStop?: () => boolean;
  concurrency?: number;
}

export interface WriteResult extends WriteProgress {
  stopped: boolean;
}

/** Write `files` below `root`, creating folders as needed; a file that fails is reported and the rest go on. */
export async function writeFolderExport(o: WriteOptions): Promise<WriteResult> {
  const dirs = new Map<string, Promise<DirHandle>>([["", Promise.resolve(o.root)]]);
  const dirFor = (dir: string): Promise<DirHandle> => {
    let handle = dirs.get(dir);
    if (!handle) {
      handle = dirFor(parentOf(dir)).then((parent) => parent.getDirectoryHandle(nameOf(dir), { create: true }));
      dirs.set(dir, handle);
    }
    return handle;
  };
  const failures: WriteFailure[] = [];
  let done = 0;
  let bytes = 0;
  let stopped = false;
  await pool(o.files, o.concurrency ?? 4, async (f) => {
    if (stopped || o.shouldStop?.()) {
      stopped = true;
      return;
    }
    try {
      const data = await o.fetchBytes(f.rel);
      const dir = await dirFor(parentOf(f.rel));
      const handle = await dir.getFileHandle(nameOf(f.rel), { create: true });
      const writable = await handle.createWritable();
      try {
        await writable.write(data);
        await writable.close();
      } catch (e) {
        await writable.abort?.(e).catch(() => undefined);
        throw e;
      }
      bytes += data.byteLength;
    } catch (e) {
      failures.push({ rel: f.rel, reason: e instanceof Error ? e.message : String(e) });
    }
    done += 1;
    o.onProgress?.({ done, total: o.files.length, bytes, failures });
  });
  return { done, total: o.files.length, bytes, failures, stopped };
}
