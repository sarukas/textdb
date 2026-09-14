import { describe, expect, it } from "vitest";
import { compareVersions, describeLineEndings, lineEndings } from "./eol";

describe("line endings", () => {
  it("names how a text ends its lines", () => {
    expect(lineEndings("a\nb\n")).toBe("lf");
    expect(lineEndings("a\r\nb\r\n")).toBe("crlf");
    expect(lineEndings("a\r\nb\nc")).toBe("mixed");
    expect(lineEndings("no newline")).toBe("none");
  });
});

describe("compareVersions", () => {
  it("does not show lines whose only change is the line ending, but counts them", () => {
    // An agent wrote LF lines into a CRLF file; a later sync made them CRLF again.
    const before = "---\r\ntitle: RBM\r\nstatus: draft\n---\r\n# RBM\r\n\r\nOne.\nTwo.\r\n";
    const after = "---\r\ntitle: RBM\r\nstatus: draft\r\n---\r\n# RBM\r\n\r\nOne.\r\nTwo.\r\n";
    const c = compareVersions(before, after);
    expect(c).toMatchObject({ hunks: 0, added: 0, removed: 0, lineEndingOnly: 2, endingsA: "mixed", endingsB: "crlf" });
    expect(c.a).toBe(c.b);
    expect(describeLineEndings(c)).toBe("2 lines differ only in line endings (mixed → CRLF)");
  });

  it("shows real changes and counts ending-only lines around them", () => {
    const before = "a\r\nb\r\nc\r\nd\r\n";
    const after = "a\nB\nc\r\nd\n";
    const c = compareVersions(before, after);
    expect(c).toMatchObject({ hunks: 1, added: 1, removed: 1, lineEndingOnly: 2 });
    expect(c.b).toBe("a\nB\nc\nd\n");
  });

  it("says nothing when the endings agree", () => {
    const c = compareVersions("x\r\ny\r\n", "x\r\nz\r\n");
    expect(c).toMatchObject({ hunks: 1, lineEndingOnly: 0 });
    expect(describeLineEndings(c)).toBeNull();
  });
});
