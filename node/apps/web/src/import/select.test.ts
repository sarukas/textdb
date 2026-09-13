import { describe, expect, it } from "vitest";
import {
  countExtensions,
  destPath,
  formatBytes,
  includeFile,
  NO_EXTENSION,
  normalizePrefix,
  parseExtensions,
  toggleExtension,
} from "./select";

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

describe("extensions found in a folder", () => {
  const files = [
    { rel: "a.md", size: 10 },
    { rel: "docs/b.MD", size: 20 },
    { rel: "docs/c.pdf", size: 1000 },
    { rel: "Makefile", size: 5 },
    { rel: "odd.", size: 1 },
    { rel: ".git/config", size: 3 },
    { rel: "node_modules/x/readme.md", size: 7 },
    { rel: "docs/.hidden.txt", size: 2 },
  ];

  it("counts what an import could read, most files first", () => {
    expect(countExtensions(files)).toEqual([
      { ext: "md", files: 2, bytes: 30 },
      { ext: NO_EXTENSION, files: 2, bytes: 6 },
      { ext: "pdf", files: 1, bytes: 1000 },
    ]);
    expect(countExtensions([{ rel: "a.md", size: null }, { rel: "b.md", size: 3 }])).toEqual([{ ext: "md", files: 2, bytes: null }]);
  });

  it("selects files without an extension only when asked", () => {
    expect(includeFile("Makefile", ["md"])).toBe(false);
    expect(includeFile("Makefile", [NO_EXTENSION])).toBe(true);
    expect(parseExtensions(`md, ${NO_EXTENSION}`)).toEqual(["md", NO_EXTENSION]);
  });

  it("toggles one extension, including out of *", () => {
    expect(toggleExtension(["md"], "txt", true, [])).toEqual(["md", "txt"]);
    expect(toggleExtension(["md", "txt"], "md", false, [])).toEqual(["txt"]);
    expect(toggleExtension(["*"], "pdf", false, ["md", "pdf", NO_EXTENSION])).toEqual(["md", NO_EXTENSION]);
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

describe("formatBytes", () => {
  it("scales units", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(300 * 1024 * 1024)).toBe("300 MB");
  });
});
