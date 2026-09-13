/**
 * Reading a folder the user picks in the browser. Chrome and Edge offer the File System
 * Access API, which walks the folder lazily and lets hidden folders be skipped without
 * reading them; elsewhere a `webkitdirectory` file input hands over every file at once.
 *
 * Listing a folder only reads names. A file is opened when it is imported: opening one can
 * fail or take a long time (an online-only cloud file downloads first, a locked file refuses),
 * and one such file must not stop the listing of thousands of others.
 */
import { skipDirectory } from "./select";

export interface PickedFile {
  /** Path inside the picked folder, `/`-separated. */
  rel: string;
  /** Known up front only for a `webkitdirectory` input; a listed handle is sized when opened. */
  size: number | null;
  open: () => Promise<File>;
}

export interface Unreadable {
  /** The folder or file inside the picked folder, `/`-separated; "" is the picked folder. */
  rel: string;
  reason: string;
}

export interface PickedFolder {
  name: string;
  /** Every visible file, sorted by path; type filtering happens later. */
  files: PickedFile[];
  /** Folders that could not be listed (their files are missing) and entries that were refused. */
  unreadable: Unreadable[];
}

export interface DirectoryHandle {
  kind: "directory";
  name: string;
  values(): AsyncIterable<DirectoryHandle | FileHandle>;
}

export interface FileHandle {
  kind: "file";
  name: string;
  getFile(): Promise<File>;
}

type DirectoryPicker = (options: { mode: "read" }) => Promise<DirectoryHandle>;

/** Folders listed at the same time. */
const WALKERS = 8;
/** How often, at most, the running count is reported. */
const REPORT_MS = 100;

function directoryPicker(): DirectoryPicker | null {
  const picker = (globalThis as unknown as { showDirectoryPicker?: DirectoryPicker }).showDirectoryPicker;
  return typeof picker === "function" ? picker.bind(globalThis) : null;
}

export function canPickDirectory(): boolean {
  return directoryPicker() !== null;
}

export const errorText = (e: unknown) =>
  e instanceof DOMException ? `${e.name}: ${e.message}` : e instanceof Error ? e.message : String(e);

function byPath<T extends { rel: string }>(a: T, b: T): number {
  return a.rel < b.rel ? -1 : a.rel > b.rel ? 1 : 0;
}

/**
 * Ask for a folder and list it, reporting how many files have been found so far. Resolves to
 * `null` when the user dismisses the picker or `signal` aborts the walk.
 */
export async function pickFolder(onFound: (count: number) => void, signal: AbortSignal): Promise<PickedFolder | null> {
  const picker = directoryPicker();
  if (!picker) throw new Error("this browser cannot pick folders directly");
  let root: DirectoryHandle;
  try {
    root = await picker({ mode: "read" });
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") return null;
    throw error;
  }
  return listFolder(root, onFound, signal);
}

/**
 * List every visible file under `root`, a few folders at a time. A folder that cannot be
 * listed, or stops listing part way, is recorded and the walk goes on.
 */
export async function listFolder(
  root: DirectoryHandle,
  onFound: (count: number) => void,
  signal: AbortSignal,
): Promise<PickedFolder | null> {
  const files: PickedFile[] = [];
  const unreadable: Unreadable[] = [];
  const queue: { dir: DirectoryHandle; prefix: string }[] = [{ dir: root, prefix: "" }];
  let active = 0;
  let reported = 0;
  const waiting: (() => void)[] = [];
  const wakeAll = () => waiting.splice(0).forEach((resume) => resume());

  const report = () => {
    const now = Date.now();
    if (now - reported >= REPORT_MS) {
      reported = now;
      onFound(files.length);
    }
  };

  const list = async (dir: DirectoryHandle, prefix: string) => {
    try {
      for await (const entry of dir.values()) {
        if (signal.aborted) return;
        const rel = prefix + entry.name;
        if (entry.kind === "directory") {
          if (!skipDirectory(entry.name)) {
            queue.push({ dir: entry, prefix: `${rel}/` });
            wakeAll();
          }
        } else if (entry.kind === "file") {
          files.push({ rel, size: null, open: () => entry.getFile() });
          report();
        } else {
          unreadable.push({ rel, reason: `not a file or folder (${String((entry as { kind: unknown }).kind)})` });
        }
      }
    } catch (e) {
      unreadable.push({ rel: prefix.replace(/\/$/, ""), reason: `folder could not be listed: ${errorText(e)}` });
    }
  };

  // A small pool: each worker takes a folder off the queue; an idle worker waits until a busy
  // one finds more folders or finishes, and all stop once the queue is empty and nobody lists.
  const worker = async () => {
    for (;;) {
      if (signal.aborted) return;
      const next = queue.pop();
      if (next) {
        active++;
        await list(next.dir, next.prefix);
        active--;
        wakeAll();
      } else if (active === 0) {
        wakeAll();
        return;
      } else {
        await new Promise<void>((resume) => waiting.push(resume));
      }
    }
  };
  await Promise.all(Array.from({ length: WALKERS }, worker));

  if (signal.aborted) return null;
  onFound(files.length);
  return { name: root.name, files: files.sort(byPath), unreadable: unreadable.sort(byPath) };
}

/** The files of a `webkitdirectory` input, relative to the folder that was picked. */
export function folderFromFileList(list: FileList | readonly File[]): PickedFolder | null {
  const files: PickedFile[] = [];
  let name = "";
  for (const file of Array.from(list)) {
    const parts = file.webkitRelativePath.split("/");
    name ||= parts[0] ?? "";
    const inside = parts.slice(1);
    if (inside.length === 0 || inside.slice(0, -1).some(skipDirectory)) continue;
    files.push({ rel: inside.join("/"), size: file.size, open: () => Promise.resolve(file) });
  }
  return name ? { name, files: files.sort(byPath), unreadable: [] } : null;
}

/**
 * A file's content as text, or why it is skipped. The HTTP API carries text, so a binary file
 * (a NUL in its first 8 KB) or one that is not valid UTF-8 is left out rather than mangled.
 * A byte-order mark is kept: the store round-trips bytes exactly.
 */
export async function readText(file: File): Promise<{ text: string } | { skip: string }> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  if (bytes.subarray(0, 8192).includes(0)) return { skip: "binary file" };
  try {
    return { text: new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes) };
  } catch {
    return { skip: "not valid UTF-8" };
  }
}
