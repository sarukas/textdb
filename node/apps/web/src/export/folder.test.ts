import { describe, expect, it } from "vitest";
import { planFolderExport, writeFolderExport, type DirHandle, type ExportFile, type FileHandleLike } from "./folder";
import type { Platform } from "./names";

const encode = (s: string) => new TextEncoder().encode(s);
const decode = (b: Uint8Array) => new TextDecoder().decode(b);

/** An in-memory folder with the parts of the File System Access API an export uses. */
class FakeFile implements FileHandleLike {
  readonly kind = "file" as const;
  writes = 0;
  constructor(
    readonly name: string,
    public data: Uint8Array,
  ) {}
  async getFile() {
    return new Blob([this.data as BlobPart]);
  }
  async createWritable() {
    const parts: Uint8Array[] = [];
    return {
      write: async (chunk: Uint8Array) => void parts.push(chunk),
      close: async () => {
        this.data = encode(parts.map(decode).join(""));
        this.writes += 1;
      },
    };
  }
}

class FakeDir implements DirHandle {
  readonly kind = "directory" as const;
  readonly entries = new Map<string, FakeDir | FakeFile>();
  constructor(readonly name: string) {}
  async *values() {
    yield* this.entries.values();
  }
  async getDirectoryHandle(name: string, options?: { create?: boolean }) {
    const e = this.entries.get(name);
    if (e instanceof FakeDir) return e;
    if (e || !options?.create) throw new DOMException(name, e ? "TypeMismatchError" : "NotFoundError");
    const d = new FakeDir(name);
    this.entries.set(name, d);
    return d;
  }
  async getFileHandle(name: string, options?: { create?: boolean }) {
    const e = this.entries.get(name);
    if (e instanceof FakeFile) return e;
    if (e || !options?.create) throw new DOMException(name, e ? "TypeMismatchError" : "NotFoundError");
    const f = new FakeFile(name, new Uint8Array());
    this.entries.set(name, f);
    return f;
  }
  file(rel: string, text: string) {
    const parts = rel.split("/");
    let dir: FakeDir = this;
    for (const seg of parts.slice(0, -1)) {
      let next = dir.entries.get(seg);
      if (!(next instanceof FakeDir)) dir.entries.set(seg, (next = new FakeDir(seg)));
      dir = next;
    }
    const f = new FakeFile(parts[parts.length - 1]!, encode(text));
    dir.entries.set(f.name, f);
    return f;
  }
  at(rel: string): FakeDir | FakeFile | undefined {
    let cur: FakeDir | FakeFile | undefined = this;
    for (const seg of rel.split("/")) cur = cur instanceof FakeDir ? cur.entries.get(seg) : undefined;
    return cur;
  }
}

function setup(store: Record<string, string>, platform: Platform, root: FakeDir) {
  const files: ExportFile[] = Object.entries(store).map(([rel, text]) => ({ rel, nbytes: encode(text).byteLength }));
  return planFolderExport({
    root,
    files,
    platform,
    // The "hash" is the content itself: enough to tell equal from different.
    storeHashes: async (rels) => new Map(rels.map((r) => [r, store[r]!])),
    diskHash: (blob) => blob.text(),
  });
}

describe("export into a folder", () => {
  it("plans new, changed and unchanged files, and writes only what differs", async () => {
    const root = new FakeDir("checkout");
    const same = root.file("a.md", "same\r\n");
    root.file("b.md", "diff");
    root.file("sub/c.md", "short");
    const extra = root.file("keep.txt", "not in textdb");
    root.file(".git/HEAD", "ref: refs/heads/main");
    const store = { "a.md": "same\r\n", "b.md": "DIFF", "sub/c.md": "longer content", "new/deep/d.md": "x" };

    const plan = await setup(store, "linux", root);
    expect(plan.problems).toEqual([]);
    expect(Object.fromEntries(plan.files.map((f) => [f.rel, f.change]))).toEqual({
      "a.md": "unchanged",
      "b.md": "changed",
      "sub/c.md": "changed",
      "new/deep/d.md": "new",
    });
    expect(plan.counts).toEqual({ new: 1, changed: 2, unchanged: 1 });

    const result = await writeFolderExport({
      root,
      files: plan.files.filter((f) => f.change !== "unchanged"),
      fetchBytes: async (rel) => encode(store[rel as keyof typeof store]),
    });
    expect([result.done, result.failures]).toEqual([3, []]);
    expect(same.writes).toBe(0);
    expect(decode((root.at("b.md") as FakeFile).data)).toBe("DIFF");
    expect(decode((root.at("new/deep/d.md") as FakeFile).data)).toBe("x");
    expect(root.at("keep.txt")).toBe(extra);
    expect(root.at(".git/HEAD")).toBeInstanceOf(FakeFile);
  });

  it("stops on names that exist on disk in another letter case, where case does not count", async () => {
    const root = new FakeDir("checkout");
    root.file("Readme.md", "old");
    root.file("Guides/intro.md", "old");
    const store = { "README.md": "new", "guides/intro.md": "new" };

    const onWindows = await setup(store, "windows", root);
    expect(onWindows.problems.map((p) => [p.path, p.kind, p.blocking])).toEqual([
      ["guides/", "disk-case", true],
      ["README.md", "disk-case", true],
    ]);
    const onLinux = await setup(store, "linux", root);
    expect(onLinux.problems).toEqual([]);
    expect(onLinux.counts.new).toBe(2);
  });

  it("stops where a file on disk stands in for a folder, or the reverse", async () => {
    const root = new FakeDir("checkout");
    root.file("docs", "a file");
    root.file("notes.md/inner.md", "a folder named like a file");
    const plan = await setup({ "docs/x.md": "x", "notes.md": "n" }, "linux", root);
    expect(plan.problems.map((p) => [p.path, p.kind])).toEqual([
      ["docs/", "disk-kind"],
      ["notes.md", "disk-kind"],
    ]);
  });
});
