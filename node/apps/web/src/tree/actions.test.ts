import { describe, expect, it } from "vitest";
import { actionFor, checkMove, describeStat, nameRange } from "./actions";

describe("checkMove", () => {
  it("normalises the target path", () => {
    expect(checkMove("/a/b.md", " a//c.md/ ")).toEqual({ ok: true, to: "/a/c.md" });
    expect(checkMove("/a/b.md", "/elsewhere/deep/b.md")).toEqual({ ok: true, to: "/elsewhere/deep/b.md" });
  });

  it("treats an unchanged path as nothing to do, not an error", () => {
    expect(checkMove("/a/b.md", "/a/b.md/")).toEqual({ ok: false, reason: null });
  });

  it("refuses empty, root, dot segments and moving a folder inside itself", () => {
    expect(checkMove("/a", "  ")).toMatchObject({ ok: false, reason: "Enter the new path." });
    expect(checkMove("/a", "/")).toMatchObject({ ok: false });
    expect(checkMove("/a", "/b/../c")).toMatchObject({ ok: false });
    expect(checkMove("/a", "/a/inner")).toMatchObject({ ok: false, reason: "A folder cannot move inside itself." });
    expect(checkMove("/a", "/ab")).toEqual({ ok: true, to: "/ab" });
  });
});

describe("nameRange", () => {
  it("selects a file's name without its extension, and a folder's whole name", () => {
    const pick = (path: string, kind: "file" | "folder") => path.slice(...nameRange(path, kind));
    expect(pick("/docs/guide.v2.md", "file")).toBe("guide.v2");
    expect(pick("/docs/.env", "file")).toBe(".env");
    expect(pick("/docs/README", "file")).toBe("README");
    expect(pick("/docs/v1.2", "folder")).toBe("v1.2");
  });
});

describe("describeStat", () => {
  it("counts a subtree", () => {
    expect(describeStat({ path: "/a", kind: "folder", files: 120, folders: 1, nbytes: 2048 })).toBe(
      "120 files, 1 subfolder, 2.0 KB",
    );
    expect(describeStat({ path: "/a/b.md", kind: "file", files: 1, folders: 0, nbytes: 12 })).toBe("1 file, 12 B");
  });
});

describe("actionFor", () => {
  it("keeps only file and folder kinds", () => {
    expect(actionFor("delete", { path: "/x", kind: "folder" })).toEqual({ op: "delete", path: "/x", kind: "folder" });
    expect(actionFor("move", { path: "/y.md", kind: "file" }).kind).toBe("file");
  });
});
