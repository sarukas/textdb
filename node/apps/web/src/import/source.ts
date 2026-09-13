/**
 * Reading a folder the user picks in the browser. Chrome and Edge offer the File System
 * Access API, which walks the folder lazily and lets hidden folders be skipped without
 * reading them; elsewhere a `webkitdirectory` file input hands over every file at once.
 */
import { skipDirectory } from "./select";

export interface PickedFile {
  /** Path inside the picked folder, `/`-separated. */
  rel: string;
  size: number;
  file: File;
}

export interface PickedFolder {
  name: string;
  /** Every visible file, sorted by path; type filtering happens later. */
  files: PickedFile[];
}

interface DirectoryHandle {
  kind: "directory";
  name: string;
  values(): AsyncIterable<DirectoryHandle | FileHandle>;
}

interface FileHandle {
  kind: "file";
  name: string;
  getFile(): Promise<File>;
}

type DirectoryPicker = (options: { mode: "read" }) => Promise<DirectoryHandle>;

function directoryPicker(): DirectoryPicker | null {
  const picker = (window as unknown as { showDirectoryPicker?: DirectoryPicker }).showDirectoryPicker;
  return typeof picker === "function" ? picker.bind(window) : null;
}

export function canPickDirectory(): boolean {
  return directoryPicker() !== null;
}

function byPath(a: PickedFile, b: PickedFile): number {
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
  const files: PickedFile[] = [];
  const walk = async (dir: DirectoryHandle, prefix: string): Promise<void> => {
    for await (const entry of dir.values()) {
      if (signal.aborted) return;
      if (entry.kind === "directory") {
        if (!skipDirectory(entry.name)) await walk(entry, `${prefix}${entry.name}/`);
      } else {
        const file = await entry.getFile();
        files.push({ rel: prefix + entry.name, size: file.size, file });
        if (files.length % 250 === 0) onFound(files.length);
      }
    }
  };
  await walk(root, "");
  if (signal.aborted) return null;
  onFound(files.length);
  return { name: root.name, files: files.sort(byPath) };
}

/** The files of a `webkitdirectory` input, relative to the folder that was picked. */
export function folderFromFileList(list: FileList): PickedFolder | null {
  const files: PickedFile[] = [];
  let name = "";
  for (const file of Array.from(list)) {
    const parts = file.webkitRelativePath.split("/");
    name ||= parts[0] ?? "";
    const inside = parts.slice(1);
    if (inside.length === 0 || inside.slice(0, -1).some(skipDirectory)) continue;
    files.push({ rel: inside.join("/"), size: file.size, file });
  }
  return name ? { name, files: files.sort(byPath) } : null;
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
