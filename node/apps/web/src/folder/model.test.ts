import { describe, expect, it } from "vitest";
import type { ChangeEvent } from "../api";
import {
  extensionOf,
  nextSort,
  pagesIn,
  parseFilter,
  planChange,
  readColumns,
  topLevel,
} from "./model";

function change(op: ChangeEvent["op"], path: string, old_path: string | null = null): ChangeEvent {
  return {
    seq: 1,
    ts: "2026-09-13T12:00:00Z",
    op,
    path,
    old_path,
    node_kind: path.endsWith(".md") ? "file" : "folder",
    version: null,
    base_version: null,
    commit_kind: null,
    author: "ann",
    message: null,
  };
}

describe("filter syntax", () => {
  it("separates keys from the name part", () => {
    expect(parseFilter("guide author:ann type:.md is:file")).toEqual({ name: "guide", author: "ann", type: "md", kind: "file" });
    expect(parseFilter('by:"Ann Lee" *.txt')).toEqual({ author: "Ann Lee", name: "*.txt" });
    expect(parseFilter("is:thing notes")).toEqual({ name: "is:thing notes" });
    expect(parseFilter("   ")).toEqual({});
  });
});

describe("columns and sorting", () => {
  it("keeps the name column and falls back to the default", () => {
    expect(readColumns('["size","bogus"]')).toEqual(["name", "size"]);
    expect(readColumns("not json")).toContain("authors");
  });

  it("starts numbers and dates at the top, flips on a second click", () => {
    expect(nextSort({ key: "name", order: "asc" }, "size")).toEqual({ key: "size", order: "desc" });
    expect(nextSort({ key: "size", order: "desc" }, "size")).toEqual({ key: "size", order: "asc" });
    expect(nextSort({ key: "size", order: "desc" }, "type")).toEqual({ key: "type", order: "asc" });
  });

  it("derives extensions like the store", () => {
    expect(extensionOf("Guide.MD", "file")).toBe("md");
    expect(extensionOf(".gitignore", "file")).toBe("gitignore");
    expect(extensionOf("Makefile", "file")).toBe("");
    expect(extensionOf("v1.2", "folder")).toBe("");
  });
});

describe("paging and selection", () => {
  it("lists the pages a row range needs", () => {
    expect(pagesIn(0, 50)).toEqual([0]);
    expect(pagesIn(190, 410)).toEqual([0, 1, 2]);
    expect(pagesIn(400, 400)).toEqual([]);
  });

  it("drops paths inside another selected folder", () => {
    expect(topLevel(["/a/b.md", "/a", "/c.md", "/ab.md"])).toEqual(["/a", "/ab.md", "/c.md"]);
  });
});

describe("what a change does to a listing", () => {
  it("refreshes the listed row that contains a commit", () => {
    expect(planChange("/docs", false, change("commit", "/docs/a.md")).refresh).toEqual(["/docs/a.md"]);
    expect(planChange("/docs", false, change("commit", "/docs/deep/er/b.md")).refresh).toEqual(["/docs/deep"]);
    expect(planChange("/", false, change("commit", "/docs/deep/b.md")).refresh).toEqual(["/docs"]);
    expect(planChange("/docs", true, change("commit", "/docs/deep/er/b.md")).refresh.sort()).toEqual([
      "/docs/deep",
      "/docs/deep/er",
      "/docs/deep/er/b.md",
    ]);
    expect(planChange("/docs", false, change("commit", "/other/a.md")).refresh).toEqual([]);
  });

  it("counts what appears and marks what goes", () => {
    expect(planChange("/docs", false, change("create", "/docs/new.md"))).toMatchObject({ added: 1, refresh: [] });
    expect(planChange("/docs", false, change("create", "/docs/sub/new.md"))).toMatchObject({ added: 0, refresh: ["/docs/sub"] });
    expect(planChange("/docs", false, change("delete", "/docs/old.md"))).toMatchObject({ removed: ["/docs/old.md"] });
    const moved = planChange("/docs", false, change("move", "/docs/b.md", "/docs/a.md"));
    expect(moved).toMatchObject({ removed: ["/docs/a.md"], added: 1 });
    expect(planChange("/docs", false, change("move", "/elsewhere/a.md", "/docs/sub/a.md"))).toMatchObject({
      removed: [],
      added: 0,
      refresh: ["/docs/sub"],
    });
  });

  it("follows the folder itself", () => {
    expect(planChange("/docs/sub", false, change("move", "/archive/docs", "/docs")).folder).toEqual({ to: "/archive/docs/sub" });
    expect(planChange("/docs/sub", false, change("delete", "/docs")).folder).toEqual({ to: null });
    expect(planChange("/docs", false, change("delete", "/docs-old")).folder).toBeNull();
  });
});
