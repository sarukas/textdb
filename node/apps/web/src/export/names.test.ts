import { describe, expect, it } from "vitest";
import { checkNames, detectPlatform, foldName } from "./names";

const at = (problems: ReturnType<typeof checkNames>, path: string) => problems.filter((p) => p.path === path).map((p) => [p.kind, p.blocking]);

describe("export names", () => {
  it("finds names that differ only in letter case, folders included", () => {
    const problems = checkNames(["Docs/a.md", "docs/b.md", "README.md", "readme.md", "ok.md"], "linux");
    expect(at(problems, "Docs/")).toEqual([["case", false]]);
    expect(at(problems, "docs/")).toEqual([["case", false]]);
    expect(at(problems, "readme.md")).toEqual([["case", false]]);
    expect(at(problems, "ok.md")).toEqual([]);
    expect(checkNames(["A.md", "a.md"], "windows").every((p) => p.blocking)).toBe(true);
    expect(checkNames(["A.md", "a.md"], "macos").every((p) => p.blocking)).toBe(true);
  });

  it("finds names that are the same once Unicode is normalized, a problem on macOS", () => {
    const nfc = "café.md";
    const nfd = "café.md";
    expect(at(checkNames([nfc, nfd], "macos"), nfc)).toEqual([["unicode", true]]);
    expect(at(checkNames([nfc, nfd], "windows"), nfd)).toEqual([["unicode", false]]);
    expect(foldName(nfd, "macos")).toBe(foldName(nfc, "macos"));
    expect(foldName("A", "linux")).toBe("A");
  });

  it("finds names Windows refuses", () => {
    const problems = checkNames(["notes/CON.md", "aux", "com1.txt", "com10.md", "a:b.md", "folder./x.md", "tab\there.md"], "windows");
    expect(at(problems, "notes/CON.md")).toEqual([["reserved", true]]);
    expect(at(problems, "aux")).toEqual([["reserved", true]]);
    expect(at(problems, "com1.txt")).toEqual([["reserved", true]]);
    expect(at(problems, "com10.md")).toEqual([]);
    expect(at(problems, "a:b.md")).toEqual([["character", true]]);
    expect(at(problems, "tab\there.md")).toEqual([["character", true]]);
    expect(at(problems, "folder./")).toEqual([["trailing", true]]);
    expect(checkNames(["notes/CON.md"], "linux").every((p) => !p.blocking)).toBe(true);
    expect(at(checkNames([`${"x/".repeat(130)}a.md`], "windows"), `${"x/".repeat(130)}a.md`)).toEqual([["long", true]]);
  });

  it("lists blocking problems first", () => {
    const problems = checkNames(["b/CON.md", "A.md", "a.md"], "macos");
    expect(problems.map((p) => p.blocking)).toEqual([true, true, false]);
  });

  it("tells the platform from the browser", () => {
    expect(detectPlatform({ userAgentData: { platform: "Windows" } })).toBe("windows");
    expect(detectPlatform({ platform: "MacIntel" })).toBe("macos");
    expect(detectPlatform({ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)" })).toBe("linux");
  });
});
