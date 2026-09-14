import { describe, expect, it } from "vitest";
import { folderFromFileList, listFolder, type DirectoryHandle, type FileHandle } from "./source";

type Tree = { [name: string]: string | Tree | Error };

/** A fake directory handle; an `Error` value makes listing that folder fail at that entry. */
function dir(name: string, tree: Tree): DirectoryHandle {
  return {
    kind: "directory",
    name,
    async *values() {
      for (const [child, value] of Object.entries(tree)) {
        await Promise.resolve();
        if (value instanceof Error) throw value;
        if (typeof value === "string") {
          const handle: FileHandle = {
            kind: "file",
            name: child,
            getFile: async () => new File([value], child),
          };
          yield handle;
        } else {
          yield dir(child, value);
        }
      }
    },
  };
}

const list = (root: DirectoryHandle, signal = new AbortController().signal) => listFolder(root, () => {}, signal);

describe("listFolder", () => {
  it("lists files sorted by path, skipping .git, .trash and node_modules", async () => {
    const folder = await list(
      dir("kb", {
        "b.md": "b",
        a: { "x.md": "x", deep: { "y.md": "y" } },
        ".git": { config: "c" },
        ".trash": { "old.md": "o" },
        ".claude": { "rules.md": "r" },
        node_modules: { "z.md": "z" },
        ".hidden.md": "h",
      }),
    );
    expect(folder?.name).toBe("kb");
    expect(folder?.files.map((f) => f.rel)).toEqual([".claude/rules.md", ".hidden.md", "a/deep/y.md", "a/x.md", "b.md"]);
    expect(folder?.unreadable).toEqual([]);
    expect(await (await folder!.files[2]!.open()).text()).toBe("y");
  });

  it("keeps going when a folder cannot be listed part way", async () => {
    const folder = await list(
      dir("kb", {
        good: { "1.md": "1", "2.md": "2" },
        bad: { "before.md": "b", boom: new DOMException("gone", "NotFoundError"), "after.md": "a" },
        "top.md": "t",
      }),
    );
    expect(folder?.files.map((f) => f.rel)).toEqual(["bad/before.md", "good/1.md", "good/2.md", "top.md"]);
    expect(folder?.unreadable).toEqual([{ rel: "bad", reason: "folder could not be listed: NotFoundError: gone" }]);
  });

  it("does not open files while listing", async () => {
    let opened = 0;
    const root: DirectoryHandle = {
      kind: "directory",
      name: "kb",
      async *values() {
        for (let i = 0; i < 5; i++) {
          yield { kind: "file", name: `${i}.md`, getFile: () => (opened++, Promise.reject(new Error("offline"))) };
        }
      },
    };
    const folder = await list(root);
    expect(folder?.files).toHaveLength(5);
    expect(opened).toBe(0);
  });

  it("lists thousands of files across many folders", async () => {
    const tree: Tree = {};
    for (let d = 0; d < 60; d++) {
      const sub: Tree = {};
      for (let f = 0; f < 100; f++) sub[`f${f}.md`] = "";
      tree[`d${d}`] = { nested: sub, "index.md": "" };
    }
    const folder = await list(dir("kb", tree));
    expect(folder?.files).toHaveLength(60 * 101);
  });

  it("resolves to null when aborted", async () => {
    const ctl = new AbortController();
    ctl.abort();
    expect(await list(dir("kb", { "a.md": "a" }), ctl.signal)).toBeNull();
  });
});

describe("folderFromFileList", () => {
  it("strips the picked folder and skips .git", () => {
    const file = (rel: string) => Object.assign(new File(["x"], rel.split("/").pop()!), { webkitRelativePath: rel });
    const folder = folderFromFileList([file("kb/b.md"), file("kb/.git/HEAD"), file("kb/a/c.md"), file("kb/.obsidian/n.md")]);
    expect(folder?.name).toBe("kb");
    expect(folder?.files.map((f) => [f.rel, f.size])).toEqual([
      [".obsidian/n.md", 1],
      ["a/c.md", 1],
      ["b.md", 1],
    ]);
  });
});
