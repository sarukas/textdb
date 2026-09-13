import { describe, expect, it } from "vitest";
import { folderPath, hashFor, parseHash } from "./hash";

describe("location hash", () => {
  it("tells files, folders and trash apart", () => {
    expect(parseHash("#/guides/intro.md")).toEqual({ type: "file", path: "/guides/intro.md", line: null });
    expect(parseHash("#/guides/intro.md:42")).toEqual({ type: "file", path: "/guides/intro.md", line: 42 });
    expect(parseHash("#/guides/")).toEqual({ type: "folder", path: "/guides" });
    expect(parseHash("#/")).toEqual({ type: "folder", path: "/" });
    expect(parseHash("#trash:8144")).toEqual({ type: "trash", id: 8144 });
    expect(parseHash("")).toBeNull();
    expect(parseHash("#%E0%A4%A")).toBeNull();
  });

  it("round-trips names that need encoding", () => {
    for (const target of [
      { type: "file", path: "/notes/a b ü.md", line: 3 },
      { type: "folder", path: "/team docs/2026" },
      { type: "folder", path: "/" },
    ] as const) {
      expect(parseHash(hashFor(target))).toEqual(target);
    }
  });

  it("normalises folder paths", () => {
    expect(folderPath("/a//b/")).toBe("/a/b");
    expect(folderPath("")).toBe("/");
  });
});
