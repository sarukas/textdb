import { describe, expect, it } from "vitest";
import { destPath, formatBytes, includeFile, normalizePrefix, parseExtensions, planBatches } from "./select";

const MD = ["md", "markdown"];

describe("includeFile", () => {
  it("takes files with a listed extension, in any case", () => {
    expect(includeFile("guide/intro.md", MD)).toBe(true);
    expect(includeFile("README.MD", MD)).toBe(true);
    expect(includeFile("notes.markdown", MD)).toBe(true);
    expect(includeFile("logo.png", MD)).toBe(false);
    expect(includeFile("md", MD)).toBe(false);
  });

  it("never reads hidden files, hidden folders or node_modules", () => {
    expect(includeFile(".git/HEAD.md", MD)).toBe(false);
    expect(includeFile("docs/.drafts/a.md", MD)).toBe(false);
    expect(includeFile("site/node_modules/pkg/README.md", MD)).toBe(false);
    expect(includeFile("docs/.hidden.md", MD)).toBe(false);
    expect(includeFile("docs/.hidden.md", ["*"])).toBe(false);
  });

  it("takes every visible file for *", () => {
    expect(includeFile("data/table.csv", ["*"])).toBe(true);
    expect(includeFile("LICENSE", ["*"])).toBe(true);
  });
});

describe("parseExtensions", () => {
  it("accepts dots, globs, commas and spaces", () => {
    expect(parseExtensions("md, .Markdown *.txt;md")).toEqual(["md", "markdown", "txt"]);
    expect(parseExtensions("  ")).toEqual([]);
  });
});

describe("normalizePrefix / destPath", () => {
  it("normalises slashes and refuses dot segments", () => {
    expect(normalizePrefix(" docs/guides/ ")).toBe("/docs/guides");
    expect(normalizePrefix("\\docs\\x")).toBe("/docs/x");
    expect(normalizePrefix("")).toBe("/");
    expect(normalizePrefix("/docs/../etc")).toBeNull();
  });

  it("joins the folder and the relative path", () => {
    expect(destPath("/", "a/b.md")).toBe("/a/b.md");
    expect(destPath("/docs", "a/b.md")).toBe("/docs/a/b.md");
  });
});

describe("planBatches", () => {
  const files = (...sizes: number[]) => sizes.map((size, i) => ({ id: i, size }));
  const ids = (batches: { id: number }[][]) => batches.map((b) => b.map((f) => f.id));

  it("fills a request up to the byte budget, keeping order", () => {
    expect(ids(planBatches(files(40, 40, 40, 10), 100, 10))).toEqual([[0, 1], [2, 3]]);
  });

  it("caps the number of files per request", () => {
    expect(ids(planBatches(files(1, 1, 1, 1, 1), 100, 2))).toEqual([[0, 1], [2, 3], [4]]);
  });

  it("sends a file larger than the budget on its own", () => {
    expect(ids(planBatches(files(10, 500, 10), 100, 10))).toEqual([[0], [1], [2]]);
    expect(planBatches([], 100, 10)).toEqual([]);
  });
});

describe("formatBytes", () => {
  it("scales units", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(300 * 1024 * 1024)).toBe("300 MB");
  });
});
