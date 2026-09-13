import { describe, expect, it } from "vitest";
import { authorHue } from "./color";
import { ancestorsOf, baseName, isWithin, parentOf } from "./paths";

describe("paths", () => {
  it("navigates store paths", () => {
    expect(parentOf("/a/b/c.md")).toBe("/a/b");
    expect(parentOf("/c.md")).toBe("/");
    expect(baseName("/a/b/c.md")).toBe("c.md");
    expect(ancestorsOf("/a/b/c.md")).toEqual(["/a", "/a/b"]);
    expect(ancestorsOf("/c.md")).toEqual([]);
    expect(isWithin("/a", "/a/b.md")).toBe(true);
    expect(isWithin("/a", "/ab/b.md")).toBe(false);
    expect(isWithin("/", "/x")).toBe(true);
  });
});

describe("authorHue", () => {
  it("is deterministic and spreads similar names", () => {
    expect(authorHue("agent-7")).toBe(authorHue("agent-7"));
    expect(authorHue("human")).toBeGreaterThanOrEqual(0);
    expect(authorHue("human")).toBeLessThan(360);
    const hues = ["agent-1", "agent-2", "agent-3", "agent-4"].map(authorHue);
    expect(new Set(hues).size).toBe(4);
  });
});
